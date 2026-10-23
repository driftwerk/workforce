//! What a turn offers the model: the tools it uses with their full
//! definitions, and every other tool as a catalogue line whose definition the
//! model loads with `tool_load` and calls through `tool_call`
//!.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use workforce_common::session::estimate_tokens;
use workforce_common::settings::ToolLoadingMode;

use crate::llm::anthropic::CACHE_BREAK_MARKER;
use crate::llm::provider::ToolDeferral;
use crate::llm::types::LlmToolDef;
use crate::runner::to_llm_name;
use crate::tools::dispatch::ToolExecution;
use crate::tools::ToolDefinition;

/// The runner's own tool for loading catalogued tools.
pub const LOAD_TOOL: &str = "tool.load";

/// The runner's own tool for calling a loaded tool, so the declared tools stay
/// the same for the whole turn on every wire.
pub const CALL_TOOL: &str = "tool.call";

/// What the runtime tells a model to call without the model choosing it from
/// the catalogue: the identity prompt (`memory`, `notes`), compaction markers
/// (`tool.retrieve`), staged-table envelopes (`data`), the advisor directive,
/// the standing preference instruction and the recall fallback (`sessions`).
pub const ALWAYS_RESIDENT: [&str; 7] = [
    "memory",
    "notes",
    "tool.retrieve",
    "data",
    "advisor",
    "remember_preference",
    "sessions",
];

/// A model with a smaller window than this loads on demand whatever the mode.
const LARGE_CONTEXT_TOKENS: u32 = 200_000;

/// A list this short in tools and tokens is offered whole: its catalogue
/// would save little.
const SHORT_LIST_TOOLS: usize = 40;
const SHORT_LIST_TOKENS: u32 = 40_000;

/// A used tool brings the rest of its namespace when the namespace is at most
/// this large, so siblings are chosen between on full descriptions.
const WHOLE_NAMESPACE_TOOLS: usize = 30;

/// An MCP server this large is loaded on demand whatever the mode.
const LARGE_MCP_TOOL_COUNT: usize = 50;
const LARGE_MCP_SCHEMA_TOKENS: u32 = 40_000;

const SUMMARY_MAX_CHARS: usize = 160;

/// How an agent's turns load tools: the configured mode, the tools the agent
/// has been using (`None` until its usage has been read) and the tools its
/// configuration keeps resident.
#[derive(Debug, Clone, Default)]
pub struct ToolLoading {
    pub mode: ToolLoadingMode,
    pub core: Option<Arc<BTreeSet<String>>>,
    pub always: Arc<BTreeSet<String>>,
}

/// Everything a plan is worked out from besides the tools themselves.
#[derive(Debug, Clone, Copy)]
pub struct PlanContext<'a> {
    pub loading: &'a ToolLoading,
    /// Tools the conversation used or loaded in its recent turns.
    pub session: &'a BTreeSet<String>,
    /// The smallest context window the request can reach.
    pub context_floor: Option<u32>,
    /// The registry is a deliberate subset with no catalogue path.
    pub precurated: bool,
}

/// Which tools a plan may defer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    Nothing,
    Everything,
    Servers(BTreeSet<String>),
}

/// A turn's tools: resident, loaded during the turn, or catalogued.
#[derive(Debug, Clone, Default)]
pub struct ToolPlan {
    resident: Vec<ToolDefinition>,
    deferred: Vec<ToolDefinition>,
    loaded: Vec<String>,
    catalogue: Option<String>,
}

/// What a load request came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    /// These tools, by internal name, are now loaded.
    Loaded(Vec<String>),
    /// Every requested tool was already offered in full.
    AlreadyAvailable,
    /// These requested names are not tools of this plan; nothing was loaded.
    Unknown(Vec<String>),
}

impl LoadOutcome {
    /// What the model reads as the load's result.
    pub fn message(&self) -> String {
        match self {
            Self::Loaded(names) => format!(
                "Loaded: {}. Call them with `{}`, or directly if they are in your tool list.",
                names
                    .iter()
                    .map(|name| to_llm_name(name))
                    .collect::<Vec<_>>()
                    .join(", "),
                to_llm_name(CALL_TOOL)
            ),
            Self::AlreadyAvailable => "Already in your tool list; call them directly.".into(),
            Self::Unknown(names) if names.is_empty() => {
                "No tool names given. Pass the exact names from the tool catalogue.".into()
            }
            Self::Unknown(names) => format!(
                "Unknown tool name(s): {}. Use exact names from the tool catalogue.",
                names.join(", ")
            ),
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Unknown(_))
    }

    /// The tools this outcome loaded, by internal name.
    pub fn loaded(&self) -> &[String] {
        match self {
            Self::Loaded(names) => names,
            _ => &[],
        }
    }
}

/// A call the plan answers in place of running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanAnswer {
    /// The tool the transcript records: [`LOAD_TOOL`], or the unloaded tool
    /// the model called.
    pub tool: String,
    pub outcome: LoadOutcome,
    pub message: String,
    pub is_error: bool,
}

impl PlanAnswer {
    /// The answer as the record of a call that ran no tool.
    pub fn execution(self) -> ToolExecution {
        ToolExecution {
            output: serde_json::json!({ "loaded": self.outcome.loaded() }),
            success: !self.is_error,
            duration_ms: 0,
            is_error: self.is_error,
            deny_reason: None,
            loads: self.outcome.loaded().to_vec(),
            output_text: self.message,
            rate_limited: None,
        }
    }
}

/// The message a call to a catalogued tool that was never loaded gets back in
/// place of running: the tool is loaded by the attempt.
pub fn unloaded_call_message(tool: &str) -> String {
    format!(
        "`{}` was not loaded, so it did not run. It is loaded now; call it again, with `{}` or \
         directly if it is in your tool list, with arguments that match its definition.",
        to_llm_name(tool),
        to_llm_name(CALL_TOOL)
    )
}

/// A load result's text followed by the definitions of the tools it loaded,
/// for every wire that does not load them natively.
pub fn with_definitions(note: &str, definitions: &[ToolDefinition]) -> String {
    let definitions: Vec<serde_json::Value> = definitions
        .iter()
        .map(|def| {
            serde_json::json!({
                "name": to_llm_name(&def.name),
                "description": def.description,
                "input_schema": def.input_schema,
            })
        })
        .collect();
    format!(
        "{note}\n\n{}",
        serde_json::to_string(&definitions).unwrap_or_default()
    )
}

/// A `tool_call`, unwrapped: the tool it names and its arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct UnwrappedCall {
    pub tool: String,
    pub arguments: serde_json::Value,
}

/// Unwrap a call to [`CALL_TOOL`]; `None` for any other call. An error names
/// what is missing from the call.
pub fn unwrap_call(
    llm_name: &str,
    input: &serde_json::Value,
) -> Option<Result<UnwrappedCall, String>> {
    (llm_name == CALL_TOOL || llm_name == to_llm_name(CALL_TOOL)).then(|| {
        let tool = input
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                format!(
                    "`{}` needs the `name` of a loaded tool.",
                    to_llm_name(CALL_TOOL)
                )
            })?;
        let arguments = match input.get("arguments") {
            None | Some(serde_json::Value::Null) => serde_json::json!({}),
            Some(serde_json::Value::String(text)) => serde_json::from_str(text)
                .ok()
                .filter(serde_json::Value::is_object)
                .ok_or_else(|| "`arguments` must be a JSON object.".to_string())?,
            Some(object @ serde_json::Value::Object(_)) => object.clone(),
            Some(_) => return Err("`arguments` must be a JSON object.".to_string()),
        };
        Ok(UnwrappedCall {
            tool: tool.to_string(),
            arguments,
        })
    })
}

impl ToolPlan {
    /// The plan for `definitions` — the registry's tools this caller may use,
    /// in registry order — under `ctx`.
    pub fn build(definitions: Vec<ToolDefinition>, ctx: &PlanContext<'_>) -> Self {
        let scope = scope_of(&definitions, ctx, ctx.loading.mode == ToolLoadingMode::On);
        Self::with_scope(definitions, ctx, &scope)
    }

    /// The tools a turn in `ToolLoadingMode::On` would defer, for shadowing;
    /// empty until the agent's usage has been read.
    pub fn would_defer(definitions: Vec<ToolDefinition>, ctx: &PlanContext<'_>) -> Self {
        let scope = scope_of(&definitions, ctx, true);
        Self::with_scope(definitions, ctx, &scope)
    }

    fn with_scope(definitions: Vec<ToolDefinition>, ctx: &PlanContext<'_>, scope: &Scope) -> Self {
        let resident_names = resident_names(&definitions, ctx.loading);
        let is_resident = |def: &ToolDefinition| {
            resident_names.contains(&def.name) || ctx.session.contains(&def.name)
        };
        let (resident, deferred): (Vec<_>, Vec<_>) =
            definitions.into_iter().partition(|def| match scope {
                Scope::Nothing => true,
                Scope::Everything => is_resident(def),
                Scope::Servers(servers) => {
                    mcp_server(&def.name).is_none_or(|server| !servers.contains(server))
                        || is_resident(def)
                }
            });
        let catalogue = (!deferred.is_empty()).then(|| render_catalogue(&deferred));
        Self {
            resident,
            deferred,
            loaded: Vec::new(),
            catalogue,
        }
    }

    /// Carry `previous`'s loads into this plan, for a plan rebuilt mid-turn; a
    /// tool that left the registry is dropped on its own.
    pub fn keep_loads(mut self, previous: &Self) -> Self {
        for name in &previous.loaded {
            self.load(std::slice::from_ref(name));
        }
        self
    }

    pub fn defers_anything(&self) -> bool {
        !self.deferred.is_empty()
    }

    /// Whether `tool` is catalogued and not yet loaded: calling it loads it
    /// instead of running it.
    pub fn is_unloaded(&self, tool: &str) -> bool {
        self.deferred.iter().any(|def| def.name == tool) && !self.loaded.iter().any(|t| t == tool)
    }

    /// Load `requested` tools, given by internal or LLM name. One unknown name
    /// loads nothing.
    pub fn load(&mut self, requested: &[String]) -> LoadOutcome {
        let outcome = self.resolve(requested);
        self.loaded.extend(outcome.loaded().iter().cloned());
        outcome
    }

    /// What loading `requested` would come to, leaving the plan as it is.
    fn resolve(&self, requested: &[String]) -> LoadOutcome {
        let known: BTreeMap<String, &str> = self
            .resident
            .iter()
            .chain(&self.deferred)
            .flat_map(|def| {
                [
                    (def.name.clone(), def.name.as_str()),
                    (to_llm_name(&def.name), def.name.as_str()),
                ]
            })
            .collect();
        let resolved: Vec<Option<&str>> = requested
            .iter()
            .map(|name| known.get(name).copied())
            .collect();
        let unknown: Vec<String> = requested
            .iter()
            .zip(&resolved)
            .filter(|(_, found)| found.is_none())
            .map(|(name, _)| name.clone())
            .collect();
        if requested.is_empty() || !unknown.is_empty() {
            return LoadOutcome::Unknown(unknown);
        }
        let fresh: Vec<String> = resolved
            .into_iter()
            .flatten()
            .filter(|name| self.is_unloaded(name))
            .fold(Vec::new(), |mut acc, name| {
                if !acc.iter().any(|seen: &String| seen == name) {
                    acc.push(name.to_string());
                }
                acc
            });
        match fresh.is_empty() {
            true => LoadOutcome::AlreadyAvailable,
            false => LoadOutcome::Loaded(fresh),
        }
    }

    /// Answer `tool_load` calls, and calls to catalogued tools not yet loaded,
    /// which load the tool instead of running it; `None` for every other call.
    /// `tool` is the call's resolved internal name. The plan is left as it
    /// stood when the round began: what an answer loads is applied from the
    /// next round, so no call in this round runs a tool whose definition the
    /// model has not yet been shown.
    pub fn answer(
        &self,
        agent: &str,
        round: u32,
        llm_name: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> Option<PlanAnswer> {
        if llm_name == LOAD_TOOL || llm_name == to_llm_name(LOAD_TOOL) {
            let outcome = self.resolve(&requested_names(input));
            tracing::info!(agent, round, outcome = ?outcome, "Tool load");
            return Some(PlanAnswer {
                tool: LOAD_TOOL.to_string(),
                message: outcome.message(),
                is_error: outcome.is_error(),
                outcome,
            });
        }
        self.is_unloaded(tool).then(|| {
            tracing::info!(
                agent,
                round,
                tool,
                "Unloaded tool called; loaded instead of run"
            );
            PlanAnswer {
                tool: tool.to_string(),
                outcome: self.resolve(&[tool.to_string()]),
                message: unloaded_call_message(tool),
                is_error: true,
            }
        })
    }

    /// The definitions the request declares in full: resident tools, then
    /// `tool_load` and `tool_call` when anything is catalogued. Loads never
    /// change them, so a turn's tools are the same in every round.
    pub fn declared(&self) -> Vec<LlmToolDef> {
        let meta = self
            .defers_anything()
            .then(|| [load_tool_definition(), call_tool_definition()]);
        self.resident
            .iter()
            .chain(meta.iter().flatten())
            .map(llm_definition)
            .collect()
    }

    /// What a wire that defers natively needs besides the declared tools:
    /// every catalogued tool, and which of them were loaded.
    pub fn deferral(&self) -> Option<ToolDeferral> {
        self.defers_anything().then(|| ToolDeferral {
            deferred: Arc::new(self.deferred.iter().map(llm_definition).collect()),
            loaded: Arc::new(self.loaded.iter().map(|name| to_llm_name(name)).collect()),
        })
    }

    /// `system_prompt` with the catalogue at the start of its semi-stable
    /// segment, so the static segment stays the same for every caller.
    pub fn prompt<'a>(&self, system_prompt: &'a str) -> Cow<'a, str> {
        match &self.catalogue {
            None => Cow::Borrowed(system_prompt),
            Some(catalogue) => Cow::Owned(match system_prompt.split_once(CACHE_BREAK_MARKER) {
                Some((stable, rest)) => {
                    format!("{stable}{CACHE_BREAK_MARKER}{catalogue}\n\n{rest}")
                }
                None => format!("{system_prompt}\n\n{catalogue}"),
            }),
        }
    }

    /// Tokens a request spends on tools: the declared definitions and the
    /// catalogue.
    pub fn offered_tokens(&self) -> u32 {
        self.declared()
            .iter()
            .map(definition_tokens)
            .sum::<u32>()
            .saturating_add(self.catalogue.as_deref().map_or(0, estimate_tokens))
    }

    /// Tools catalogued instead of declared, and the tokens that saves once
    /// the catalogue is paid for.
    pub fn deferred_summary(&self) -> (usize, u32) {
        let definitions: u32 = self
            .deferred
            .iter()
            .map(|def| definition_tokens(&llm_definition(def)))
            .sum();
        let catalogue = self.catalogue.as_deref().map_or(0, estimate_tokens);
        (self.deferred.len(), definitions.saturating_sub(catalogue))
    }
}

fn scope_of(definitions: &[ToolDefinition], ctx: &PlanContext<'_>, mode_on: bool) -> Scope {
    let small_context = ctx
        .context_floor
        .is_some_and(|floor| floor < LARGE_CONTEXT_TOKENS);
    let servers = large_mcp_servers(definitions);
    match () {
        _ if ctx.precurated || is_short(definitions) => Scope::Nothing,
        _ if small_context || (mode_on && ctx.loading.core.is_some()) => Scope::Everything,
        _ if !servers.is_empty() => Scope::Servers(servers),
        _ => Scope::Nothing,
    }
}

fn is_short(definitions: &[ToolDefinition]) -> bool {
    definitions.len() <= SHORT_LIST_TOOLS
        && definitions
            .iter()
            .map(|def| definition_tokens(&llm_definition(def)))
            .sum::<u32>()
            <= SHORT_LIST_TOKENS
}

/// The tools resident by the agent's usage and the always lists: those tools,
/// and the rest of each of their namespaces of at most
/// [`WHOLE_NAMESPACE_TOOLS`] tools.
fn resident_names(definitions: &[ToolDefinition], loading: &ToolLoading) -> BTreeSet<String> {
    let core = loading.core.as_deref();
    let seeds: Vec<&ToolDefinition> = definitions
        .iter()
        .filter(|def| {
            core.is_some_and(|core| core.contains(&def.name))
                || ALWAYS_RESIDENT
                    .iter()
                    .copied()
                    .chain(loading.always.iter().map(String::as_str))
                    .any(|entry| within(&def.name, entry))
        })
        .collect();
    let sizes = definitions
        .iter()
        .fold(BTreeMap::<&str, usize>::new(), |mut sizes, def| {
            *sizes.entry(namespace(&def.name)).or_default() += 1;
            sizes
        });
    let whole: BTreeSet<&str> = seeds
        .iter()
        .map(|def| namespace(&def.name))
        .filter(|ns| {
            sizes
                .get(ns)
                .is_some_and(|&size| size <= WHOLE_NAMESPACE_TOOLS)
        })
        .collect();
    definitions
        .iter()
        .filter(|def| whole.contains(namespace(&def.name)))
        .chain(seeds)
        .map(|def| def.name.clone())
        .collect()
}

/// `name` is `entry` or lies under it (`google` holds `google.drive.upload`).
fn within(name: &str, entry: &str) -> bool {
    name.strip_prefix(entry)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

/// A tool's namespace: its name without the last segment; an undotted name is
/// its own.
pub fn namespace(name: &str) -> &str {
    name.rsplit_once('.')
        .map_or(name, |(namespace, _)| namespace)
}

/// The MCP server an `mcp.<server>.<tool>` name belongs to.
fn mcp_server(name: &str) -> Option<&str> {
    name.strip_prefix("mcp.")
        .and_then(|rest| rest.split_once('.'))
        .map(|(server, _)| server)
}

fn large_mcp_servers(definitions: &[ToolDefinition]) -> BTreeSet<String> {
    definitions
        .iter()
        .filter_map(|def| mcp_server(&def.name).map(|server| (server, def)))
        .fold(
            BTreeMap::<&str, (usize, u32)>::new(),
            |mut sizes, (server, def)| {
                let size = sizes.entry(server).or_default();
                size.0 += 1;
                size.1 += definition_tokens(&llm_definition(def));
                sizes
            },
        )
        .into_iter()
        .filter(|(_, (tools, tokens))| {
            *tools > LARGE_MCP_TOOL_COUNT || *tokens > LARGE_MCP_SCHEMA_TOKENS
        })
        .map(|(server, _)| server.to_string())
        .collect()
}

fn render_catalogue(deferred: &[ToolDefinition]) -> String {
    let mut lines: Vec<String> = deferred
        .iter()
        .map(|def| {
            format!(
                "- {}: {}",
                to_llm_name(&def.name),
                summary(&def.description)
            )
        })
        .collect();
    lines.sort();
    format!(
        "## Tool catalogue\nThese tools are available but not loaded. To use one, call `{}` with its \
         exact name (several at once if the task needs them); it returns the tool's definition. Then \
         call it with `{}`, passing its name and arguments, or directly if it now appears in your \
         tool list. Tools already in your tool list need no loading.\n{}",
        to_llm_name(LOAD_TOOL),
        to_llm_name(CALL_TOOL),
        lines.join("\n")
    )
}

/// The first sentence of a description's first line, capped.
fn summary(description: &str) -> String {
    let line = description.trim().lines().next().unwrap_or_default();
    let sentence = line
        .char_indices()
        .find(|&(i, c)| {
            matches!(c, '.' | '!' | '?')
                && line[i + c.len_utf8()..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
        })
        .map_or(line, |(i, c)| &line[..i + c.len_utf8()]);
    match sentence.char_indices().nth(SUMMARY_MAX_CHARS - 1) {
        Some((cut, _)) => format!("{}…", &sentence[..cut]),
        None => sentence.to_string(),
    }
}

fn load_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: LOAD_TOOL.to_string(),
        description: "Load catalogued tools by exact name so you can call them. Load every tool \
                      the task needs in one call."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "Exact tool names from the tool catalogue."
                }
            },
            "required": ["names"]
        }),
    }
}

fn call_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: CALL_TOOL.to_string(),
        description: format!(
            "Call a tool you loaded with `{}`: its exact name and arguments that match the \
             definition `{}` returned.",
            to_llm_name(LOAD_TOOL),
            to_llm_name(LOAD_TOOL)
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Exact name of a loaded tool."
                },
                "arguments": {
                    "type": "object",
                    "description": "The tool's arguments, matching its definition."
                }
            },
            "required": ["name", "arguments"]
        }),
    }
}

fn llm_definition(def: &ToolDefinition) -> LlmToolDef {
    LlmToolDef {
        name: to_llm_name(&def.name),
        description: def.description.clone(),
        input_schema: def.input_schema.clone(),
    }
}

/// Estimated tokens of one declared definition.
pub fn definition_tokens(def: &LlmToolDef) -> u32 {
    estimate_tokens(&serde_json::to_string(def).unwrap_or_default())
}

/// The names a `tool_load` call asked for: its `names` strings, empty when
/// the input carries none.
pub fn requested_names(input: &serde_json::Value) -> Vec<String> {
    input
        .get("names")
        .and_then(serde_json::Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("Does {name} things. More detail follows."),
            input_schema: json!({ "type": "object", "properties": { "a": { "type": "string" } } }),
        }
    }

    fn defs(names: &[&str]) -> Vec<ToolDefinition> {
        names.iter().map(|name| def(name)).collect()
    }

    /// `names` and enough other tools that the list is not short.
    fn padded(names: &[&str]) -> Vec<ToolDefinition> {
        defs(names)
            .into_iter()
            .chain((0..SHORT_LIST_TOOLS).map(|i| def(&format!("zz_filler{i}"))))
            .collect()
    }

    fn loading(mode: ToolLoadingMode, core: Option<&[&str]>) -> ToolLoading {
        ToolLoading {
            mode,
            core: core.map(|names| Arc::new(names.iter().map(|n| n.to_string()).collect())),
            always: Arc::default(),
        }
    }

    fn ctx<'a>(loading: &'a ToolLoading, session: &'a BTreeSet<String>) -> PlanContext<'a> {
        PlanContext {
            loading,
            session,
            context_floor: Some(1_000_000),
            precurated: false,
        }
    }

    fn names(defs: &[LlmToolDef]) -> Vec<&str> {
        defs.iter().map(|d| d.name.as_str()).collect()
    }

    const TOOLS: [&str; 7] = [
        "github.create_issue",
        "github.pr_review",
        "memory",
        "metabase.query",
        "notes",
        "scheduler",
        "vault.set",
    ];

    #[test]
    fn with_loading_off_a_large_model_is_offered_everything_and_no_catalogue() {
        let loading = loading(ToolLoadingMode::Off, Some(&["scheduler"]));
        let session = BTreeSet::new();
        let plan = ToolPlan::build(padded(&TOOLS), &ctx(&loading, &session));
        assert_eq!(plan.declared().len(), TOOLS.len() + SHORT_LIST_TOOLS);
        assert!(plan.deferral().is_none());
        assert_eq!(plan.prompt("sys"), "sys");
    }

    #[test]
    fn with_loading_on_used_namespaces_and_the_runtimes_own_tools_are_resident() {
        let loading = loading(ToolLoadingMode::On, Some(&["github.pr_review"]));
        let session = BTreeSet::new();
        let plan = ToolPlan::build(padded(&TOOLS), &ctx(&loading, &session));
        assert_eq!(
            names(&plan.declared()),
            [
                "github_create_issue",
                "github_pr_review",
                "memory",
                "notes",
                "tool_load",
                "tool_call"
            ]
        );
        assert!(plan.is_unloaded("metabase.query"));
        assert!(!plan.is_unloaded("github.create_issue"));
    }

    #[test]
    fn with_loading_on_nothing_is_deferred_until_usage_has_been_read() {
        let loading = loading(ToolLoadingMode::On, None);
        let session = BTreeSet::new();
        let plan = ToolPlan::build(padded(&TOOLS), &ctx(&loading, &session));
        assert!(!plan.defers_anything());
    }

    #[test]
    fn the_session_and_the_configured_list_keep_tools_resident() {
        let loading = ToolLoading {
            always: Arc::new(BTreeSet::from(["metabase".to_string()])),
            ..loading(ToolLoadingMode::On, Some(&[]))
        };
        let session = BTreeSet::from(["vault.set".to_string()]);
        let plan = ToolPlan::build(padded(&TOOLS), &ctx(&loading, &session));
        let declared = plan.declared();
        let declared = names(&declared);
        assert!(declared.contains(&"metabase_query"));
        assert!(declared.contains(&"vault_set"));
        assert!(!declared.contains(&"scheduler"));
    }

    #[test]
    fn a_small_context_floor_forces_loading_even_when_off() {
        let many: Vec<String> = (0..45).map(|i| format!("tool{i}.run")).collect();
        let all: Vec<&str> = many.iter().map(String::as_str).chain(["memory"]).collect();
        let loading = loading(ToolLoadingMode::Off, None);
        let session = BTreeSet::new();
        let small = PlanContext {
            context_floor: Some(131_072),
            ..ctx(&loading, &session)
        };
        let plan = ToolPlan::build(defs(&all), &small);
        assert_eq!(
            names(&plan.declared()),
            ["memory", "tool_load", "tool_call"]
        );

        let few = ToolPlan::build(defs(&all[..40]), &small);
        assert!(!few.defers_anything(), "40 tools fit any window");

        let large = PlanContext {
            context_floor: Some(202_752),
            ..small
        };
        assert!(!ToolPlan::build(defs(&all), &large).defers_anything());
    }

    #[test]
    fn a_precurated_registry_is_offered_whole() {
        let loading = loading(ToolLoadingMode::On, Some(&[]));
        let session = BTreeSet::new();
        let plan = ToolPlan::build(
            padded(&TOOLS),
            &PlanContext {
                precurated: true,
                context_floor: Some(8_000),
                ..ctx(&loading, &session)
            },
        );
        assert!(!plan.defers_anything());
    }

    #[test]
    fn a_large_mcp_server_is_deferred_even_when_off_and_small_ones_are_not() {
        let big: Vec<String> = (0..51).map(|i| format!("mcp.crm.tool{i}")).collect();
        let all: Vec<&str> = big
            .iter()
            .map(String::as_str)
            .chain(["mcp.linear.issue_create", "memory"])
            .collect();
        let loading = loading(ToolLoadingMode::Off, None);
        let session = BTreeSet::new();
        let plan = ToolPlan::build(defs(&all), &ctx(&loading, &session));
        assert_eq!(
            names(&plan.declared()),
            [
                "mcp_linear_issue_create",
                "memory",
                "tool_load",
                "tool_call"
            ]
        );
        assert!(plan.is_unloaded("mcp.crm.tool7"));
    }

    #[test]
    fn a_few_huge_mcp_tools_defer_on_their_token_size() {
        let huge = |name: &str| ToolDefinition {
            input_schema: json!({ "description": "x".repeat(60_000) }),
            ..def(name)
        };
        let loading = loading(ToolLoadingMode::Off, None);
        let session = BTreeSet::new();
        let plan = ToolPlan::build(
            vec![
                huge("mcp.gql.a"),
                huge("mcp.gql.b"),
                huge("mcp.gql.c"),
                def("mcp.small.a"),
            ],
            &ctx(&loading, &session),
        );
        assert_eq!(
            names(&plan.declared()),
            ["mcp_small_a", "tool_load", "tool_call"]
        );
    }

    #[test]
    fn a_used_tool_brings_its_namespace_only_when_the_namespace_is_small() {
        let crm: Vec<String> = (0..51).map(|i| format!("mcp.crm.tool{i}")).collect();
        let github: Vec<String> = (0..30).map(|i| format!("github.op{i}")).collect();
        let all: Vec<&str> = crm.iter().chain(&github).map(String::as_str).collect();
        let loading = loading(ToolLoadingMode::On, Some(&["mcp.crm.tool3", "github.op0"]));
        let session = BTreeSet::new();
        let plan = ToolPlan::build(defs(&all), &ctx(&loading, &session));
        assert!(!plan.is_unloaded("mcp.crm.tool3"));
        assert!(
            plan.is_unloaded("mcp.crm.tool4"),
            "a 51-tool namespace is not whole"
        );
        assert!(
            !plan.is_unloaded("github.op29"),
            "a 30-tool namespace is whole"
        );
    }

    #[test]
    fn a_short_list_is_offered_whole_whatever_the_mode() {
        let loading = loading(ToolLoadingMode::On, Some(&[]));
        let session = BTreeSet::new();
        let small_floor = PlanContext {
            context_floor: Some(8_000),
            ..ctx(&loading, &session)
        };
        assert!(!ToolPlan::build(defs(&TOOLS), &small_floor).defers_anything());
        assert!(ToolPlan::build(padded(&TOOLS), &small_floor).defers_anything());
    }

    #[test]
    fn a_load_leaves_the_declared_tools_as_they_were() {
        let loading = loading(ToolLoadingMode::On, Some(&[]));
        let session = BTreeSet::new();
        let mut plan = ToolPlan::build(padded(&TOOLS), &ctx(&loading, &session));
        let before = plan.declared();
        let outcome = plan.load(&[
            "vault_set".into(),
            "github.pr_review".into(),
            "vault.set".into(),
        ]);
        assert_eq!(
            outcome,
            LoadOutcome::Loaded(vec!["vault.set".into(), "github.pr_review".into()])
        );
        assert!(outcome
            .message()
            .starts_with("Loaded: vault_set, github_pr_review."));
        assert_eq!(names(&plan.declared()), names(&before));
        assert_eq!(
            names(&before),
            ["memory", "notes", "tool_load", "tool_call"]
        );
        assert!(!plan.is_unloaded("vault.set"));
        assert!(plan.is_unloaded("github.create_issue"), "loads are exact");
        let deferral = plan.deferral().unwrap();
        assert_eq!(
            *deferral.loaded,
            BTreeSet::from(["vault_set".to_string(), "github_pr_review".to_string()])
        );
        assert!(deferral.deferred.iter().any(|d| d.name == "vault_set"));
        assert_eq!(deferral.deferred.len(), plan.deferred.len());
    }

    #[test]
    fn a_load_with_any_unknown_name_loads_nothing() {
        let loading = loading(ToolLoadingMode::On, Some(&[]));
        let session = BTreeSet::new();
        let mut plan = ToolPlan::build(padded(&TOOLS), &ctx(&loading, &session));
        let outcome = plan.load(&["vault_set".into(), "vault_sett".into()]);
        assert_eq!(outcome, LoadOutcome::Unknown(vec!["vault_sett".into()]));
        assert!(outcome.is_error());
        assert!(plan.is_unloaded("vault.set"));
        assert!(plan.load(&[]).is_error());
        assert!(LoadOutcome::Unknown(Vec::new())
            .message()
            .contains("No tool names"));
    }

    #[test]
    fn loading_a_resident_or_loaded_tool_reports_it_already_available() {
        let loading = loading(ToolLoadingMode::On, Some(&[]));
        let session = BTreeSet::new();
        let mut plan = ToolPlan::build(padded(&TOOLS), &ctx(&loading, &session));
        assert_eq!(plan.load(&["memory".into()]), LoadOutcome::AlreadyAvailable);
        plan.load(&["scheduler".into()]);
        assert_eq!(
            plan.load(&["scheduler".into()]),
            LoadOutcome::AlreadyAvailable
        );
    }

    #[test]
    fn a_rebuilt_plan_keeps_the_turns_loads_that_still_exist() {
        let loading = loading(ToolLoadingMode::On, Some(&[]));
        let session = BTreeSet::new();
        let mut before = ToolPlan::build(padded(&TOOLS), &ctx(&loading, &session));
        before.load(&["scheduler".into(), "vault.set".into()]);
        let after =
            ToolPlan::build(padded(&TOOLS[..6]), &ctx(&loading, &session)).keep_loads(&before);
        assert!(!after.is_unloaded("scheduler"));
        assert_eq!(after.loaded, ["scheduler"]);
    }

    #[test]
    fn the_catalogue_lists_each_deferred_tool_once_sorted_with_its_first_sentence() {
        let loading = loading(ToolLoadingMode::On, Some(&[]));
        let session = BTreeSet::new();
        let plan = ToolPlan::build(
            padded(&["vault.set", "scheduler", "memory"]),
            &ctx(&loading, &session),
        );
        let prompt = plan.prompt("static");
        let lines: Vec<&str> = prompt
            .lines()
            .filter(|l| l.starts_with("- ") && !l.contains("filler"))
            .collect();
        assert_eq!(
            lines,
            [
                "- scheduler: Does scheduler things.",
                "- vault_set: Does vault.set things."
            ]
        );
        assert!(prompt.contains("call `tool_load`") && prompt.contains("with `tool_call`"));
    }

    #[test]
    fn the_catalogue_opens_the_semi_stable_segment() {
        let loading = loading(ToolLoadingMode::On, Some(&[]));
        let session = BTreeSet::new();
        let plan = ToolPlan::build(padded(&["scheduler", "memory"]), &ctx(&loading, &session));
        let system = format!("static{CACHE_BREAK_MARKER}semi{CACHE_BREAK_MARKER}dynamic");
        let prompt = plan.prompt(&system);
        let segments: Vec<&str> = prompt.split(CACHE_BREAK_MARKER).collect();
        assert_eq!(segments[0], "static");
        assert!(segments[1].starts_with("## Tool catalogue") && segments[1].ends_with("\n\nsemi"));
        assert_eq!(segments[2], "dynamic");
        assert!(plan
            .prompt("whole")
            .starts_with("whole\n\n## Tool catalogue"));
    }

    #[test]
    fn summaries_stop_at_the_first_sentence_and_are_capped() {
        assert_eq!(
            summary("Search CRM records. Supports filters."),
            "Search CRM records."
        );
        assert_eq!(
            summary("Version 1.5 search\nsecond line"),
            "Version 1.5 search"
        );
        let long = "a".repeat(400);
        assert_eq!(summary(&long).chars().count(), SUMMARY_MAX_CHARS);
    }

    #[test]
    fn the_data_namespace_is_resident_and_datadog_is_not() {
        let loading = loading(ToolLoadingMode::On, Some(&[]));
        let session = BTreeSet::new();
        let plan = ToolPlan::build(
            padded(&[
                "data.query",
                "data.inspect",
                "datadog.logs_search",
                "tool.retrieve",
            ]),
            &ctx(&loading, &session),
        );
        assert_eq!(
            names(&plan.declared()),
            [
                "data_query",
                "data_inspect",
                "tool_retrieve",
                "tool_load",
                "tool_call"
            ]
        );
    }

    #[test]
    fn the_deferred_summary_counts_what_the_catalogue_saves() {
        let on = loading(ToolLoadingMode::On, Some(&[]));
        let session = BTreeSet::new();
        let plan = ToolPlan::build(padded(&TOOLS), &ctx(&on, &session));
        let (count, saved) = plan.deferred_summary();
        assert_eq!(count, 5 + SHORT_LIST_TOOLS);
        let everything: u32 = plan
            .deferred
            .iter()
            .map(|d| definition_tokens(&llm_definition(d)))
            .sum();
        assert!(saved > 0 && saved < everything);
        let everything = loading(ToolLoadingMode::Off, None);
        let whole = ToolPlan::build(padded(&TOOLS), &ctx(&everything, &session));
        assert!(plan.offered_tokens() < whole.offered_tokens());
        assert_eq!(
            plan.offered_tokens(),
            plan.declared().iter().map(definition_tokens).sum::<u32>()
                + estimate_tokens(plan.catalogue.as_deref().unwrap())
        );
    }

    #[test]
    fn requested_names_reads_only_strings() {
        assert_eq!(
            requested_names(&json!({ "names": ["a", 3, "b"] })),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(requested_names(&json!({ "name": "a" })).is_empty());
    }

    #[test]
    fn a_tool_load_call_is_answered_by_the_plan() {
        let loading = loading(ToolLoadingMode::On, Some(&[]));
        let session = BTreeSet::new();
        let plan = ToolPlan::build(padded(&TOOLS), &ctx(&loading, &session));
        let answer = plan
            .answer(
                "a",
                1,
                "tool_load",
                "tool_load",
                &json!({ "names": ["scheduler"] }),
            )
            .unwrap();
        assert_eq!(answer.tool, LOAD_TOOL);
        assert!(!answer.is_error);
        let execution = answer.execution();
        assert_eq!(execution.loads, vec!["scheduler".to_string()]);
        assert_eq!(execution.output, json!({ "loaded": ["scheduler"] }));
        assert!(execution.success);

        let unknown = plan
            .answer(
                "a",
                1,
                "tool_load",
                "tool_load",
                &json!({ "names": ["nope"] }),
            )
            .unwrap();
        assert!(unknown.is_error);
        assert!(unknown.execution().loads.is_empty());
    }

    #[test]
    fn a_call_to_an_unloaded_tool_loads_it_instead_of_running() {
        let loading = loading(ToolLoadingMode::On, Some(&[]));
        let session = BTreeSet::new();
        let mut plan = ToolPlan::build(padded(&TOOLS), &ctx(&loading, &session));
        let answer = plan
            .answer("a", 1, "vault_set", "vault.set", &json!({ "key": "x" }))
            .unwrap();
        assert_eq!(answer.tool, "vault.set");
        assert!(answer.is_error);
        assert!(answer.message.starts_with("`vault_set` was not loaded"));
        assert_eq!(
            answer.outcome,
            LoadOutcome::Loaded(vec!["vault.set".into()])
        );
        assert!(
            plan.answer("a", 1, "vault_set", "vault.set", &json!({}))
                .is_some(),
            "a second call in the same round is still guarded"
        );
        plan.load(answer.outcome.loaded());
        assert!(
            plan.answer("a", 2, "vault_set", "vault.set", &json!({}))
                .is_none(),
            "loaded from the next round, the call runs"
        );
        assert!(plan
            .answer("a", 1, "memory", "memory", &json!({}))
            .is_none());
    }

    #[test]
    fn a_tool_call_unwraps_to_its_tool_and_arguments() {
        let unwrap = |input| unwrap_call("tool_call", &input).unwrap();
        assert_eq!(
            unwrap(json!({ "name": "vault_set", "arguments": { "key": "k" } })),
            Ok(UnwrappedCall {
                tool: "vault_set".into(),
                arguments: json!({ "key": "k" })
            })
        );
        assert_eq!(
            unwrap(json!({ "name": "vault_set", "arguments": "{\"key\": \"k\"}" }))
                .unwrap()
                .arguments,
            json!({ "key": "k" })
        );
        assert_eq!(
            unwrap(json!({ "name": "memory" })).unwrap().arguments,
            json!({})
        );
        assert!(unwrap(json!({ "arguments": {} }))
            .unwrap_err()
            .contains("`name`"));
        assert!(unwrap(json!({ "name": "x", "arguments": [1] })).is_err());
        assert!(unwrap(json!({ "name": "x", "arguments": "[1]" })).is_err());
        assert!(unwrap_call("tool.call", &json!({ "name": "x" })).is_some());
        assert!(unwrap_call("vault_set", &json!({})).is_none());
    }

    #[test]
    fn definitions_follow_a_load_results_text() {
        let content = with_definitions("Loaded: vault_set.", &defs(&["vault.set"]));
        let (note, json) = content.split_once("\n\n").unwrap();
        assert_eq!(note, "Loaded: vault_set.");
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed[0]["name"], "vault_set");
        assert_eq!(
            parsed[0]["input_schema"]["properties"]["a"]["type"],
            "string"
        );
    }

    #[test]
    fn a_load_and_a_call_in_one_round_leave_the_call_guarded() {
        let loading = loading(ToolLoadingMode::On, Some(&[]));
        let session = BTreeSet::new();
        let plan = ToolPlan::build(padded(&TOOLS), &ctx(&loading, &session));
        let load = plan
            .answer(
                "a",
                1,
                "tool_load",
                "tool_load",
                &json!({ "names": ["vault_set"] }),
            )
            .unwrap();
        assert_eq!(load.outcome, LoadOutcome::Loaded(vec!["vault.set".into()]));
        assert!(
            plan.answer("a", 1, "vault_set", "vault.set", &json!({}))
                .is_some(),
            "the load lands next round, so this call is loaded, not run"
        );
    }
}
