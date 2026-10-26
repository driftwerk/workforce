//! LLM dispatch for the curator. The model edits the page through tools —
//! exact-passage replacements, appends, then `finish` or `keep_page` — so a
//! reply carries changes, never the whole page, and text no edit names is
//! never re-typed. Single-pass when the sources fit the configured context
//! budget; otherwise the sources are first read in chunks into notes, and
//! the page is edited from those.
//!
//! Every stage fails closed. A reply that reaches its token cap is never
//! used: a cut-off edit reply is discarded and the model is told to make
//! smaller edits; a cut-off notes reply has its chunk halved and read again.
//! A page's edits take effect only when the model calls `finish`; a page
//! that runs out of replies is left as it was.

use std::collections::BTreeMap;
use std::sync::Arc;

use eyre::{eyre, Context, Result};
use futures::future::{try_join_all, BoxFuture};
use futures::FutureExt;
use serde_json::{json, Value};
use tracing::{info, warn};

use workforce_agent::llm::provider::{
    create_provider_async, provider_env_key, provider_env_var_hint, with_reasoning_suppressed,
    LlmProvider, ProviderKind,
};
use workforce_agent::llm::tiers::TierRegistry;
use workforce_agent::llm::types::{
    ContentBlock, LlmMessage, LlmResponse, LlmToolDef, Role, StopReason,
};
use workforce_common::settings::CuratorConfig;
use workforce_hub::vault::Vault;

use super::check::{check_page, Finding, TomlSchema};
use super::edit::PageDraft;
use super::prompt::{
    build_notes_prompt, build_prompt, build_reduce_prompt, PromptInput, SourceFile,
};

const DEFAULT_TIER_KEY: &str = "anthropic";
const MAX_MAP_CHUNKS: usize = 10;
const OVERHEAD_TOKEN_RESERVE: u32 = 15_000;
/// A notes chunk this small that still overflows its cap is not split
/// further; the page fails instead.
const MIN_SPLIT_CHARS: usize = 4_000;
/// Consecutive cut-off edit replies tolerated before the page is given up.
const MAX_CUT_OFF_REPLIES: u32 = 3;

/// What the curator decided for a page.
#[derive(Debug, PartialEq, Eq)]
pub enum Curation {
    /// The page changed; `text` is the whole edited page.
    Edited {
        text: String,
        summary: String,
        edits: usize,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        /// Examples that already failed their check before this edit and
        /// still do, for the reviewer.
        still_failing: Vec<String>,
    },
    /// The model judged the page accurate as it stands.
    Unchanged { reason: String },
}

/// Curate one page with the configured tier: resolve the provider, then
/// read the sources (in chunks when they overflow the context budget) and
/// let the model edit the page.
pub async fn curate_page(
    page: &str,
    tier_spec: &str,
    limits: &CuratorConfig,
    system_prompt: &str,
    input: &PromptInput,
    schema: TomlSchema,
    vault: Option<&Arc<Vault>>,
) -> Result<Curation> {
    let (kind, model) = resolve_tier(tier_spec)?;
    let api_key = resolve_api_key(kind, vault).ok_or_else(|| {
        eyre!(
            "no credentials for provider {:?}: set {} or vault `{}/api_key`",
            kind,
            env_var_name(kind),
            vault_key_namespace(kind),
        )
    })?;
    let provider = create_provider_async(kind, &api_key, Some(&model), None, false).await?;
    curate_with(
        &*provider,
        &model,
        page,
        limits,
        system_prompt,
        input,
        schema,
    )
    .await
}

async fn curate_with(
    provider: &dyn LlmProvider,
    model: &str,
    page: &str,
    limits: &CuratorConfig,
    system_prompt: &str,
    input: &PromptInput,
    schema: TomlSchema,
) -> Result<Curation> {
    let single_pass = build_prompt(input);
    let estimated =
        estimate_tokens(system_prompt) + estimate_tokens(&single_pass) + limits.max_tokens as usize;
    let user_prompt = if estimated <= limits.context_safety_tokens as usize {
        info!(page, %model, estimated, "curator: single-pass");
        single_pass
    } else {
        info!(
            page,
            %model,
            estimated,
            threshold = limits.context_safety_tokens,
            "curator: reading sources in chunks (input exceeds budget)"
        );
        let notes = gather_notes(page, &input.sources, limits, provider, model).await?;
        build_reduce_prompt(input, &notes)
    };
    edit_page(
        provider,
        model,
        page,
        limits,
        system_prompt,
        &user_prompt,
        input.current_content.as_deref(),
        schema,
    )
    .await
    .wrap_err_with(|| context_overflow_hint(page))
}

// ─── Notes (sources that overflow the context budget) ──────────────────────

async fn gather_notes(
    page: &str,
    sources: &[SourceFile],
    limits: &CuratorConfig,
    provider: &dyn LlmProvider,
    model: &str,
) -> Result<String> {
    let chunk_budget_chars = (limits
        .context_safety_tokens
        .saturating_sub(OVERHEAD_TOKEN_RESERVE)) as usize
        * 3;
    let chunks = split_into_chunks(sources, chunk_budget_chars);
    if chunks.len() > MAX_MAP_CHUNKS {
        return Err(eyre!(
            "page `{page}` would need {} map-passes (limit {}) — scope this \
             page's sources in docs-watch.toml or switch to a longer-context tier",
            chunks.len(),
            MAX_MAP_CHUNKS,
        ));
    }
    info!(
        page,
        chunk_count = chunks.len(),
        "curator: extracting notes per chunk"
    );
    let parts = try_join_all(
        chunks
            .into_iter()
            .map(|chunk| notes_for_chunk(page, chunk, limits.notes_max_tokens, provider, model)),
    )
    .await?;
    Ok(parts.into_iter().flatten().collect::<Vec<_>>().join("\n\n"))
}

/// Notes for one chunk of sources. A reply that reaches the cap is never
/// used: the chunk is halved and each half read again, until a chunk too
/// small to halve still overflows, which fails the page.
fn notes_for_chunk<'a>(
    page: &'a str,
    chunk: Vec<SourceFile>,
    cap: u32,
    provider: &'a dyn LlmProvider,
    model: &'a str,
) -> BoxFuture<'a, Result<Vec<String>>> {
    async move {
        let label = chunk_label(&chunk);
        let prompt = build_notes_prompt(page, &chunk, notes_word_budget(cap));
        let messages = [user_text(prompt)];
        let response = invoke(provider, model, None, &messages, &[], cap)
            .await
            .wrap_err_with(|| format!("notes extraction failed for `{page}` ({label})"))?;
        if !is_cut_off(&response, cap) {
            let notes = response.text().ok_or_else(|| {
                eyre!(
                    "notes call for `{page}` ({label}) returned no text (stop reason {:?})",
                    response.stop_reason
                )
            })?;
            return Ok(vec![notes]);
        }
        let Some((first, second)) = halve(&chunk) else {
            return Err(eyre!(
                "notes for `{page}` ({label}) overflow {cap} tokens even for a chunk of \
                 {} characters; raise `[curator] notes_max_tokens` or scope this page's \
                 sources in docs-watch.toml",
                chunk_chars(&chunk)
            ));
        };
        warn!(page, %label, cap, "notes reply reached its cap; halving the chunk");
        let (a, b) = futures::try_join!(
            notes_for_chunk(page, first, cap, provider, model),
            notes_for_chunk(page, second, cap, provider, model),
        )?;
        Ok(a.into_iter().chain(b).collect())
    }
    .boxed()
}

/// The length asked of a notes reply: well inside its token cap, so the
/// prompt never invites a reply the cap would cut off.
fn notes_word_budget(cap: u32) -> u32 {
    cap * 2 / 5
}

fn chunk_chars(chunk: &[SourceFile]) -> usize {
    chunk.iter().map(|s| s.content.len()).sum()
}

fn chunk_label(chunk: &[SourceFile]) -> String {
    match chunk {
        [one] => one.repo_relative.display().to_string(),
        many => format!(
            "{} files from {}",
            many.len(),
            many[0].repo_relative.display()
        ),
    }
}

/// Split a chunk in two: between its files when it has several, else
/// between the lines of its one file. `None` when it is too small to split.
fn halve(chunk: &[SourceFile]) -> Option<(Vec<SourceFile>, Vec<SourceFile>)> {
    if chunk_chars(chunk) < MIN_SPLIT_CHARS * 2 {
        return None;
    }
    if chunk.len() > 1 {
        let (a, b) = chunk.split_at(chunk.len() / 2);
        return Some((a.to_vec(), b.to_vec()));
    }
    let file = &chunk[0];
    let lines: Vec<&str> = file.content.split_inclusive('\n').collect();
    if lines.len() < 2 {
        return None;
    }
    let mid = lines.len() / 2;
    let part = |range: std::ops::Range<usize>| SourceFile {
        repo_relative: format!(
            "{} (lines {}–{})",
            file.repo_relative.display(),
            range.start + 1,
            range.end
        )
        .into(),
        content: lines[range].concat(),
    };
    Some((vec![part(0..mid)], vec![part(mid..lines.len())]))
}

// ─── Editing the page ──────────────────────────────────────────────────────

const CUT_OFF_NOTE: &str = "Your last reply was cut off before it ended, so none of it was \
     applied. Make smaller changes: one passage per edit call, and fewer calls per reply.";
const TOOLS_ONLY_NOTE: &str = "Reply with tool calls only: `edit` or `append` to change the \
     page, then `finish`; or `keep_page` if it needs no change.";

/// The page's examples, checked against the real CLI and config (see
/// `check`). `finish` is refused while the draft has a failing example the
/// original page didn't; failures the page already had don't block it, and
/// are reported.
struct ExampleGate {
    schema: TomlSchema,
    baseline: Vec<Finding>,
}

impl ExampleGate {
    fn new(schema: TomlSchema, original: Option<&str>) -> Self {
        let baseline = check_page(original.unwrap_or_default(), schema).findings;
        Self { schema, baseline }
    }

    /// The draft's failing examples, split into those the original page
    /// didn't have and those it did. Lines move as the page is edited, so
    /// failures are matched by example and problem, not by line.
    fn split(&self, draft: &str) -> (Vec<Finding>, Vec<Finding>) {
        let mut known: Vec<(&str, &str)> = self
            .baseline
            .iter()
            .map(|f| (f.example.as_str(), f.problem.as_str()))
            .collect();
        let mut new = Vec::new();
        let mut old = Vec::new();
        for finding in check_page(draft, self.schema).findings {
            let key = (finding.example.as_str(), finding.problem.as_str());
            match known.iter().position(|k| *k == key) {
                Some(at) => {
                    known.swap_remove(at);
                    old.push(finding);
                }
                None => new.push(finding),
            }
        }
        (new, old)
    }

    /// The note that tells the model which examples already fail.
    fn baseline_note(&self) -> Option<String> {
        (!self.baseline.is_empty()).then(|| {
            let list: Vec<String> = self.baseline.iter().map(|f| format!("- {f}")).collect();
            format!(
                "## Examples on this page that already fail their check\n\
                 Each `workforce` command is parsed with the real command line and each \
                 workforce.toml block loaded into the real config. These fail now; fix any \
                 your sources show the correct form of.\n{}\n",
                list.join("\n")
            )
        })
    }
}

/// What ends the edit of a page.
enum Verdict {
    Finish(String),
    Keep(String),
}

async fn edit_page(
    provider: &dyn LlmProvider,
    model: &str,
    page: &str,
    limits: &CuratorConfig,
    system_prompt: &str,
    user_prompt: &str,
    current: Option<&str>,
    schema: TomlSchema,
) -> Result<Curation> {
    let tools = edit_tools();
    let gate = ExampleGate::new(schema, current);
    let mut draft = PageDraft::new(current);
    let mut messages = vec![user_text(user_prompt.to_string())];
    if let Some(note) = gate.baseline_note() {
        push_user_text(&mut messages, &note);
    }
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    let mut cut_off_in_a_row = 0;

    for turn in 1..=limits.max_turns {
        let response = invoke(
            provider,
            model,
            Some(system_prompt),
            &messages,
            &tools,
            limits.max_tokens,
        )
        .await?;
        input_tokens += u64::from(response.usage.input_tokens);
        output_tokens += u64::from(response.usage.output_tokens);

        if is_cut_off(&response, limits.max_tokens) {
            cut_off_in_a_row += 1;
            warn!(
                page,
                turn,
                cap = limits.max_tokens,
                "edit reply reached its cap; discarded"
            );
            if cut_off_in_a_row >= MAX_CUT_OFF_REPLIES {
                return Err(eyre!(
                    "page `{page}`: {cut_off_in_a_row} replies in a row reached the \
                     {}-token cap; nothing written",
                    limits.max_tokens
                ));
            }
            push_user_text(&mut messages, CUT_OFF_NOTE);
            continue;
        }
        cut_off_in_a_row = 0;

        let calls: Vec<(String, String, Value)> = response
            .tool_uses()
            .into_iter()
            .map(|(id, name, input)| (id.to_string(), name.to_string(), input.clone()))
            .collect();
        messages.push(LlmMessage {
            role: Role::Assistant,
            content: response.content,
        });
        if calls.is_empty() {
            push_user_text(&mut messages, TOOLS_ONLY_NOTE);
            continue;
        }

        let (results, verdict) = apply_calls(&mut draft, &calls, &gate);
        info!(
            page,
            turn,
            calls = calls.len(),
            refused = results.iter().filter(|r| r.1.is_err()).count(),
            edits_so_far = draft.applied().len(),
            "curator: edit reply"
        );
        messages.push(LlmMessage {
            role: Role::User,
            content: results
                .into_iter()
                .map(|(id, result)| {
                    let is_error = result.is_err();
                    ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: result.unwrap_or_else(|e| e),
                        is_error: is_error.then_some(true),
                        loaded: None,
                    }
                })
                .collect(),
        });

        match verdict {
            Some(Verdict::Finish(summary)) => {
                let (_, still) = gate.split(draft.text());
                return Ok(Curation::Edited {
                    text: draft.text().to_string(),
                    summary,
                    edits: draft.applied().len(),
                    model: response.model,
                    input_tokens,
                    output_tokens,
                    still_failing: still.iter().map(ToString::to_string).collect(),
                });
            }
            Some(Verdict::Keep(reason)) => return Ok(Curation::Unchanged { reason }),
            None => {}
        }
    }
    Err(eyre!(
        "page `{page}` was not finished within {} replies; nothing written",
        limits.max_turns
    ))
}

/// Apply one reply's tool calls. Changes apply in order; `finish` and
/// `keep_page` are judged after them, and stand only when every other call
/// in the reply succeeded. `finish` also needs every example the edit
/// touched to pass its check.
fn apply_calls(
    draft: &mut PageDraft,
    calls: &[(String, String, Value)],
    gate: &ExampleGate,
) -> (Vec<(String, Result<String, String>)>, Option<Verdict>) {
    let mut results: BTreeMap<usize, (String, Result<String, String>)> = BTreeMap::new();
    let mut verdicts = Vec::new();
    for (index, (id, name, input)) in calls.iter().enumerate() {
        let result = match name.as_str() {
            "edit" => string_args(input, &["old_text", "new_text"]).and_then(|args| {
                draft
                    .replace(&args[0], &args[1])
                    .map(|()| "applied".to_string())
                    .map_err(|e| e.to_string())
            }),
            "append" => string_args(input, &["text"]).map(|args| {
                draft.append(&args[0]);
                "appended".to_string()
            }),
            "finish" | "keep_page" => {
                verdicts.push(index);
                continue;
            }
            other => Err(format!(
                "unknown tool `{other}`; use edit, append, finish or keep_page"
            )),
        };
        results.insert(index, (id.clone(), result));
    }

    let failed = results.values().any(|(_, r)| r.is_err());
    let mut verdict = None;
    for index in verdicts {
        let (id, name, input) = &calls[index];
        let result = if verdict.is_some() {
            Err("this reply already ended the edit".to_string())
        } else if failed {
            Err("a change in this reply was refused; fix it before ending the edit".to_string())
        } else if name == "finish" {
            string_args(input, &["summary"]).and_then(|args| {
                if !draft.is_changed() {
                    return Err(
                        "the page is unchanged; call keep_page if it needs no change".to_string(),
                    );
                }
                let (new, _) = gate.split(draft.text());
                if !new.is_empty() {
                    let list: Vec<String> = new.iter().map(|f| format!("- {f}")).collect();
                    return Err(format!(
                        "not finished: your edits leave {} example(s) that fail their check \
                         (each `workforce` command is parsed with the real command line, each \
                         workforce.toml block loaded into the real config). Fix them, then \
                         finish:\n{}",
                        new.len(),
                        list.join("\n")
                    ));
                }
                verdict = Some(Verdict::Finish(args[0].clone()));
                Ok("finished".to_string())
            })
        } else {
            string_args(input, &["reason"]).and_then(|args| {
                if draft.is_changed() {
                    Err("you have changed the page; finish, or undo your edits first".to_string())
                } else {
                    verdict = Some(Verdict::Keep(args[0].clone()));
                    Ok("kept".to_string())
                }
            })
        };
        results.insert(index, (id.clone(), result));
    }
    (results.into_values().collect(), verdict)
}

/// The named string arguments of a call, in order. An argument that is
/// missing is an error, never an empty value: a provider hands back `{}`
/// for arguments that didn't parse.
fn string_args(input: &Value, names: &[&str]) -> Result<Vec<String>, String> {
    names
        .iter()
        .map(|name| {
            input
                .get(*name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    format!(
                        "missing string argument `{name}`; the call may have been cut off — \
                         send it again"
                    )
                })
        })
        .collect()
}

fn edit_tools() -> Vec<LlmToolDef> {
    let tool = |name: &str, description: &str, properties: Value, required: &[&str]| LlmToolDef {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
        }),
    };
    vec![
        tool(
            "edit",
            "Replace one passage of the page. `old_text` is copied exactly from the page as \
             it stands now (your earlier edits included) and must occur exactly once; quote \
             enough surrounding text to make it unique. `new_text` replaces it; empty deletes \
             it. Keep each edit to one passage.",
            json!({
                "old_text": {"type": "string"},
                "new_text": {"type": "string"},
            }),
            &["old_text", "new_text"],
        ),
        tool(
            "append",
            "Add text to the end of the page. Use it to write a page that doesn't exist yet, \
             a section at a time.",
            json!({"text": {"type": "string"}}),
            &["text"],
        ),
        tool(
            "finish",
            "End the edit; your changes are kept. `summary` says in a sentence or two what \
             changed and why, for the reviewer.",
            json!({"summary": {"type": "string"}}),
            &["summary"],
        ),
        tool(
            "keep_page",
            "End without changing the page, because it is already accurate. `reason` says \
             why in one clause.",
            json!({"reason": {"type": "string"}}),
            &["reason"],
        ),
    ]
}

// ─── Calls ─────────────────────────────────────────────────────────────────

/// A reply that reached its cap, whatever the route reported as its stop
/// reason.
fn is_cut_off(response: &LlmResponse, cap: u32) -> bool {
    response.stop_reason == StopReason::MaxTokens || response.usage.output_tokens >= cap
}

fn user_text(text: String) -> LlmMessage {
    LlmMessage {
        role: Role::User,
        content: vec![ContentBlock::Text { text }],
    }
}

/// Add a note to the conversation's last user message, or start one.
fn push_user_text(messages: &mut Vec<LlmMessage>, text: &str) {
    match messages.last_mut() {
        Some(last) if last.role == Role::User => last.content.push(ContentBlock::Text {
            text: text.to_string(),
        }),
        _ => messages.push(user_text(text.to_string())),
    }
}

async fn invoke(
    provider: &dyn LlmProvider,
    model: &str,
    system_prompt: Option<&str>,
    messages: &[LlmMessage],
    tools: &[LlmToolDef],
    max_tokens: u32,
) -> Result<LlmResponse> {
    // The curator wants edits or notes, not a chain of thought: a model
    // that reasons by default would otherwise spend the budget on it.
    with_reasoning_suppressed(provider.create_message(
        system_prompt,
        messages,
        tools,
        Some(model),
        Some(max_tokens),
        false,
    ))
    .await
}

fn context_overflow_hint(page: &str) -> String {
    format!(
        "curating page `{page}` failed \
         (if the underlying error mentions context length, lower \
         `[curator] context_safety_tokens` so chunked reading starts sooner, \
         or scope this page's sources in docs-watch.toml)"
    )
}

/// Greedy in-order bin-packing of source files into chunks whose
/// content sizes sum to no more than `budget_chars`. A file larger
/// than the budget gets its own chunk; the provider's own context
/// error will catch it if it still doesn't fit.
pub fn split_into_chunks(sources: &[SourceFile], budget_chars: usize) -> Vec<Vec<SourceFile>> {
    let mut chunks: Vec<Vec<SourceFile>> = Vec::new();
    let mut current: Vec<SourceFile> = Vec::new();
    let mut current_size: usize = 0;
    for s in sources {
        let file_size = s.content.len();
        if !current.is_empty() && current_size + file_size > budget_chars {
            chunks.push(std::mem::take(&mut current));
            current_size = 0;
        }
        current.push(s.clone());
        current_size += file_size;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Rough token estimate via chars / 3. Conservative (real tokenizers
/// vary by model; 3.5–4 chars per token is typical for English code).
/// Using 3 errs on the high side so we trigger multi-pass sooner.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(3)
}

/// Resolve a tier spec like `"openrouter:deepseek:powerful"` into
/// `(ProviderKind, model_name)` ready for `create_provider_async`.
pub fn resolve_tier(spec: &str) -> Result<(ProviderKind, String)> {
    let registry = TierRegistry::from_defaults().wrap_err("load tier registry")?;
    let (routing_key, model) = registry.resolve_routed_model(spec, DEFAULT_TIER_KEY);
    let kind_str = routing_key.as_deref().unwrap_or(DEFAULT_TIER_KEY);
    let kind = ProviderKind::from_str_loose(kind_str)
        .ok_or_else(|| eyre!("unknown provider in tier spec `{spec}`: routing key `{kind_str}`"))?;
    if model == spec && routing_key.is_none() {
        return Err(eyre!(
            "tier spec `{spec}` did not resolve to a concrete model (check workforce_agent::llm::default_tiers.toml)",
        ));
    }
    Ok((kind, model))
}

fn resolve_api_key(kind: ProviderKind, vault: Option<&Arc<Vault>>) -> Option<String> {
    if let Some(key) = provider_env_key(kind) {
        return Some(key);
    }
    let vault = vault?;
    let path = format!("{}/api_key", vault_key_namespace(kind));
    vault.get(&path)
}

fn env_var_name(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
        ProviderKind::OpenRouter => "OPENROUTER_API_KEY",
        ProviderKind::Grok => "XAI_API_KEY",
        ProviderKind::Gemini => "GEMINI_API_KEY",
        ProviderKind::Cerebras => "CEREBRAS_API_KEY",
        ProviderKind::Zai => "ZAI_API_KEY",
        ProviderKind::Bedrock => "AWS_ACCESS_KEY_ID",
        ProviderKind::VertexAnthropic => "GOOGLE_APPLICATION_CREDENTIALS",
        ProviderKind::Ollama => "OLLAMA_URL",
        ProviderKind::Vllm => "VLLM_URL",
        ProviderKind::ChatGpt | ProviderKind::ClaudeCode | ProviderKind::Codex => {
            provider_env_var_hint(kind)
        }
    }
}

fn vault_key_namespace(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::OpenRouter => "openrouter",
        ProviderKind::Grok => "grok",
        ProviderKind::Gemini => "gemini",
        ProviderKind::Cerebras => "cerebras",
        ProviderKind::Zai => "zai",
        ProviderKind::Bedrock => "bedrock",
        ProviderKind::VertexAnthropic => "vertex",
        ProviderKind::Ollama => "ollama",
        ProviderKind::Vllm => "vllm",
        ProviderKind::ChatGpt | ProviderKind::ClaudeCode | ProviderKind::Codex => {
            kind.canonical_name()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── The edit loop, driven by a scripted provider ───────────────────────

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use workforce_agent::llm::types::TokenUsage;

    const PAGE: &str = "---\ntitle: CLI\n---\n\n## Flags\n\n- `--all` — every page.\n- `--no-pr` — no PR.\n\n## Shortcuts\n\n- `Ctrl+Y` — toggle mouse capture.\n";

    /// Replies with its scripted responses in order, and records every
    /// conversation it was sent.
    struct Scripted {
        replies: Mutex<VecDeque<LlmResponse>>,
        seen: Mutex<Vec<Vec<LlmMessage>>>,
    }

    impl Scripted {
        fn new(replies: Vec<LlmResponse>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn last_sent_text(&self) -> String {
            let seen = self.seen.lock().unwrap();
            serde_json::to_string(seen.last().unwrap()).unwrap()
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for Scripted {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            self.seen.lock().unwrap().push(messages.to_vec());
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| eyre!("script exhausted"))
        }
    }

    fn calls(list: &[(&str, Value)]) -> LlmResponse {
        reply(
            list.iter()
                .enumerate()
                .map(|(i, (name, input))| {
                    ContentBlock::tool_use(format!("c{i}"), name.to_string(), input.clone())
                })
                .collect(),
            StopReason::ToolUse,
            100,
        )
    }

    fn reply(
        content: Vec<ContentBlock>,
        stop_reason: StopReason,
        output_tokens: u32,
    ) -> LlmResponse {
        LlmResponse {
            content,
            stop_reason,
            usage: TokenUsage {
                input_tokens: 1_000,
                output_tokens,
                ..Default::default()
            },
            model: "scripted".into(),
        }
    }

    fn limits() -> CuratorConfig {
        CuratorConfig {
            max_turns: 5,
            ..CuratorConfig::default()
        }
    }

    async fn run(provider: &Scripted, current: Option<&str>) -> Result<Curation> {
        edit_page(
            provider,
            "m",
            "cli.mdx",
            &limits(),
            "system",
            "curate",
            current,
            TomlSchema::Workforce,
        )
        .await
    }

    // ── The example gate ───────────────────────────────────────────────────

    const CLI_PAGE: &str = "---\ntitle: CLI\n---\n\n## Agents\n\n```sh\nworkforce agent list\n```\n\n## Delete\n\n```sh\nworkforce agent delete --id reviewer\n```\n";

    #[tokio::test]
    async fn finish_is_refused_while_an_edit_breaks_an_example() {
        let breaks = (
            "edit",
            json!({"old_text": "workforce agent list\n", "new_text": "workforce agent list --verbose\n"}),
        );
        let repairs = (
            "edit",
            json!({"old_text": "workforce agent list --verbose\n", "new_text": "workforce agent list --workdir ./team\n"}),
        );
        let provider = Scripted::new(vec![
            calls(&[breaks, finish()]),
            calls(&[repairs, finish()]),
        ]);
        let Curation::Edited { text, .. } = run(&provider, Some(CLI_PAGE)).await.unwrap() else {
            panic!("expected an edit");
        };
        let refusal = serde_json::to_string(&provider.seen.lock().unwrap()[1]).unwrap();
        assert!(refusal.contains("not finished"), "{refusal}");
        assert!(refusal.contains("--verbose"), "{refusal}");
        assert!(text.contains("agent list --workdir ./team"));
    }

    #[tokio::test]
    async fn an_example_already_broken_does_not_block_finish_and_is_reported() {
        let unrelated = (
            "edit",
            json!({"old_text": "## Agents\n", "new_text": "## Agents\n\nList the agents.\n"}),
        );
        let provider = Scripted::new(vec![calls(&[unrelated, finish()])]);
        let Curation::Edited { still_failing, .. } = run(&provider, Some(CLI_PAGE)).await.unwrap()
        else {
            panic!("expected an edit");
        };
        assert_eq!(still_failing.len(), 1, "{still_failing:?}");
        assert!(still_failing[0].contains("agent delete --id reviewer"));
        // The model was told about it before it started.
        let first = serde_json::to_string(&provider.seen.lock().unwrap()[0]).unwrap();
        assert!(first.contains("already fail their check"), "{first}");
    }

    #[tokio::test]
    async fn fixing_a_broken_example_clears_it_from_the_report() {
        let fix = (
            "edit",
            json!({"old_text": "workforce agent delete --id reviewer", "new_text": "workforce agent delete reviewer"}),
        );
        let provider = Scripted::new(vec![calls(&[fix, finish()])]);
        let Curation::Edited { still_failing, .. } = run(&provider, Some(CLI_PAGE)).await.unwrap()
        else {
            panic!("expected an edit");
        };
        assert!(still_failing.is_empty(), "{still_failing:?}");
    }

    fn add_dry_run() -> (&'static str, Value) {
        (
            "edit",
            json!({"old_text": "- `--no-pr` — no PR.\n", "new_text": "- `--no-pr` — no PR.\n- `--dry-run` — print the plan.\n"}),
        )
    }

    fn finish() -> (&'static str, Value) {
        ("finish", json!({"summary": "Documented --dry-run."}))
    }

    #[tokio::test]
    async fn untouched_text_is_byte_identical_after_a_curation() {
        let provider = Scripted::new(vec![calls(&[add_dry_run(), finish()])]);
        let Curation::Edited {
            text,
            summary,
            edits,
            ..
        } = run(&provider, Some(PAGE)).await.unwrap()
        else {
            panic!("expected an edit");
        };
        assert_eq!(edits, 1);
        assert_eq!(summary, "Documented --dry-run.");
        let inserted = "- `--dry-run` — print the plan.\n";
        assert_eq!(text.replacen(inserted, "", 1), PAGE);
    }

    #[tokio::test]
    async fn a_cut_off_reply_is_never_applied() {
        // The cut-off reply carries a whole, valid edit; it must still be
        // thrown away, and the model told why.
        let cut = reply(
            vec![ContentBlock::tool_use(
                "c0".into(),
                "edit".into(),
                json!({"old_text": "`--all`", "new_text": "`--every`"}),
            )],
            StopReason::ToolUse,
            limits().max_tokens,
        );
        let provider = Scripted::new(vec![cut, calls(&[add_dry_run(), finish()])]);
        let Curation::Edited { text, .. } = run(&provider, Some(PAGE)).await.unwrap() else {
            panic!("expected an edit");
        };
        assert!(text.contains("`--all`"), "the cut-off edit was applied");
        assert!(provider.last_sent_text().contains("was cut off"));
    }

    #[tokio::test]
    async fn replies_cut_off_again_and_again_give_the_page_up() {
        let cut = || reply(vec![], StopReason::MaxTokens, 10);
        let provider = Scripted::new(vec![cut(), cut(), cut()]);
        let err = run(&provider, Some(PAGE)).await.unwrap_err();
        assert!(err.to_string().contains("nothing written"), "{err}");
    }

    #[tokio::test]
    async fn an_edit_that_misses_is_reported_back_and_can_be_retried() {
        let miss = (
            "edit",
            json!({"old_text": "- `--no-pr` - no PR.", "new_text": "x"}),
        );
        let provider = Scripted::new(vec![calls(&[miss]), calls(&[add_dry_run(), finish()])]);
        let Curation::Edited { text, .. } = run(&provider, Some(PAGE)).await.unwrap() else {
            panic!("expected an edit");
        };
        assert!(text.contains("--dry-run") && !text.contains("\nx"));
        let sent = provider.seen.lock().unwrap()[1].clone();
        let sent = serde_json::to_string(&sent).unwrap();
        assert!(sent.contains("does not occur in the page"), "{sent}");
    }

    #[tokio::test]
    async fn finish_is_refused_in_a_reply_whose_edit_failed() {
        let miss = ("edit", json!({"old_text": "nowhere", "new_text": "x"}));
        let provider = Scripted::new(vec![
            calls(&[add_dry_run(), miss, finish()]),
            calls(&[finish()]),
        ]);
        let Curation::Edited { edits, .. } = run(&provider, Some(PAGE)).await.unwrap() else {
            panic!("expected an edit");
        };
        assert_eq!(edits, 1);
        assert_eq!(
            provider.seen.lock().unwrap().len(),
            2,
            "the first finish must not have ended it"
        );
        assert!(provider
            .last_sent_text()
            .contains("fix it before ending the edit"));
    }

    #[tokio::test]
    async fn missing_arguments_are_an_error_not_an_empty_edit() {
        // A provider hands back `{}` for arguments that didn't parse.
        let provider = Scripted::new(vec![
            calls(&[("edit", json!({}))]),
            calls(&[add_dry_run(), finish()]),
        ]);
        run(&provider, Some(PAGE)).await.unwrap();
        let sent = serde_json::to_string(&provider.seen.lock().unwrap()[1]).unwrap();
        assert!(
            sent.contains("missing string argument `old_text`"),
            "{sent}"
        );
    }

    #[tokio::test]
    async fn no_finish_means_nothing_is_written() {
        let provider = Scripted::new(
            (0..limits().max_turns)
                .map(|_| calls(&[add_dry_run()]))
                .collect(),
        );
        let err = run(&provider, Some(PAGE)).await.unwrap_err();
        assert!(
            err.to_string().contains("not finished within 5 replies"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn keep_page_leaves_the_page_unchanged() {
        let provider = Scripted::new(vec![calls(&[(
            "keep_page",
            json!({"reason": "already accurate"}),
        )])]);
        assert_eq!(
            run(&provider, Some(PAGE)).await.unwrap(),
            Curation::Unchanged {
                reason: "already accurate".into()
            }
        );
    }

    #[tokio::test]
    async fn keep_page_is_refused_after_the_page_changed() {
        let provider = Scripted::new(vec![
            calls(&[add_dry_run(), ("keep_page", json!({"reason": "fine"}))]),
            calls(&[finish()]),
        ]);
        assert!(matches!(
            run(&provider, Some(PAGE)).await.unwrap(),
            Curation::Edited { .. }
        ));
    }

    #[tokio::test]
    async fn finish_without_a_change_is_refused() {
        let provider = Scripted::new(vec![
            calls(&[finish()]),
            calls(&[("keep_page", json!({"reason": "nothing to add"}))]),
        ]);
        assert!(matches!(
            run(&provider, Some(PAGE)).await.unwrap(),
            Curation::Unchanged { .. }
        ));
    }

    #[tokio::test]
    async fn a_reply_with_no_tool_calls_is_asked_for_tools() {
        let prose = reply(
            vec![ContentBlock::Text {
                text: "Here is the page: ...".into(),
            }],
            StopReason::EndTurn,
            50,
        );
        let provider = Scripted::new(vec![prose, calls(&[add_dry_run(), finish()])]);
        run(&provider, Some(PAGE)).await.unwrap();
        assert!(provider
            .last_sent_text()
            .contains("Reply with tool calls only"));
    }

    #[tokio::test]
    async fn a_new_page_is_written_with_appends() {
        let provider = Scripted::new(vec![calls(&[
            ("append", json!({"text": "---\ntitle: New\n---\n"})),
            ("append", json!({"text": "## Usage\n\nRun it.\n"})),
            ("finish", json!({"summary": "New page."})),
        ])]);
        let Curation::Edited { text, .. } = run(&provider, None).await.unwrap() else {
            panic!("expected an edit");
        };
        assert_eq!(text, "---\ntitle: New\n---\n## Usage\n\nRun it.\n");
    }

    // ── Notes ──────────────────────────────────────────────────────────────

    fn notes(text: &str, output_tokens: u32) -> LlmResponse {
        reply(
            vec![ContentBlock::Text { text: text.into() }],
            StopReason::EndTurn,
            output_tokens,
        )
    }

    #[tokio::test]
    async fn an_overflowing_notes_chunk_is_halved_and_read_again() {
        let chunk = vec![src("a.rs", MIN_SPLIT_CHARS), src("b.rs", MIN_SPLIT_CHARS)];
        let cap = 4_000;
        let provider = Scripted::new(vec![notes("cut", cap), notes("- a", 10), notes("- b", 10)]);
        let parts = notes_for_chunk("cli.mdx", chunk, cap, &provider, "m")
            .await
            .unwrap();
        assert_eq!(parts, vec!["- a".to_string(), "- b".to_string()]);
        assert_eq!(provider.seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_chunk_too_small_to_halve_fails_the_page() {
        let cap = 4_000;
        let provider = Scripted::new(vec![notes("cut", cap)]);
        let err = notes_for_chunk("cli.mdx", vec![src("a.rs", 100)], cap, &provider, "m")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("overflow 4000 tokens"), "{err}");
    }

    #[test]
    fn a_single_file_halves_between_its_lines() {
        let file = SourceFile {
            repo_relative: "big.rs".into(),
            content: "x\n".repeat(MIN_SPLIT_CHARS),
        };
        let (a, b) = halve(std::slice::from_ref(&file)).unwrap();
        assert_eq!(a[0].content.clone() + &b[0].content, file.content);
        assert!(a[0]
            .repo_relative
            .display()
            .to_string()
            .starts_with("big.rs (lines 1–"));
    }

    #[test]
    fn the_notes_prompt_asks_for_less_than_its_cap_allows() {
        // ~1.3 tokens a word: the asked length must leave the cap headroom.
        assert!(notes_word_budget(4_000) as f64 * 1.3 < 4_000.0 * 0.6);
    }

    #[test]
    fn resolves_openrouter_deepseek_powerful() {
        let (kind, model) = resolve_tier("openrouter:deepseek:powerful").unwrap();
        assert_eq!(kind, ProviderKind::OpenRouter);
        assert!(
            model.contains("deepseek"),
            "expected deepseek model, got {model}"
        );
    }

    #[test]
    fn resolves_anthropic_powerful() {
        let (kind, model) = resolve_tier("anthropic:powerful").unwrap();
        assert_eq!(kind, ProviderKind::Anthropic);
        assert!(model.contains("opus") || model.contains("claude"));
    }

    #[test]
    fn resolve_tier_fails_on_unknown_spec() {
        let err = resolve_tier("nonsense:bogus:powerful").unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("unknown provider") || msg.contains("did not resolve"),
            "unexpected error: {msg}",
        );
    }

    fn src(name: &str, size: usize) -> SourceFile {
        SourceFile {
            repo_relative: std::path::PathBuf::from(name),
            content: "x".repeat(size),
        }
    }

    #[test]
    fn estimate_tokens_uses_chars_over_3() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 2);
        assert_eq!(estimate_tokens("a".repeat(300).as_str()), 100);
    }

    #[test]
    fn split_into_chunks_returns_single_chunk_when_under_budget() {
        let sources = vec![src("a", 100), src("b", 200)];
        let chunks = split_into_chunks(&sources, 1000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 2);
    }

    #[test]
    fn split_into_chunks_packs_greedily_in_order() {
        let sources = vec![src("a", 40), src("b", 80), src("c", 60)];
        let chunks = split_into_chunks(&sources, 100);
        assert_eq!(chunks.len(), 3);
        let names: Vec<String> = chunks
            .iter()
            .flatten()
            .map(|s| s.repo_relative.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn split_into_chunks_packs_multiple_files_when_combined_fits() {
        let sources = vec![src("a", 30), src("b", 50), src("c", 90)];
        let chunks = split_into_chunks(&sources, 100);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 2);
        assert_eq!(chunks[1].len(), 1);
    }

    #[test]
    fn split_into_chunks_oversize_single_file_stands_alone() {
        let sources = vec![src("a", 200), src("b", 50)];
        let chunks = split_into_chunks(&sources, 100);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0][0].content.len(), 200);
        assert_eq!(chunks[1][0].content.len(), 50);
    }

    #[test]
    fn split_into_chunks_empty_input_yields_no_chunks() {
        assert!(split_into_chunks(&[], 100).is_empty());
    }
}
