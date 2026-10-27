//! LLM dispatch for the curator. The model edits the page through tools —
//! lookups in the repository, exact-passage replacements and appends that
//! cite their evidence, then `finish` or `keep_page` — so a reply carries
//! changes, never the whole page, and text no edit names is never re-typed.
//! A page's sources are included in the prompt when they fit the context
//! budget; otherwise the prompt lists them and the model looks up what it
//! needs.
//!
//! Every stage fails closed. A reply that reaches its token cap is never
//! used: it is discarded and the model is told to make smaller edits. A
//! change applies only when its evidence is in the sources word for word
//! and the verifier finds it supported. A page's edits take effect only
//! when the model calls `finish`; a page that runs out of replies is left
//! as it was.

use std::collections::BTreeMap;
use std::sync::Arc;

use eyre::{eyre, Context, Result};
use serde_json::{json, Value};
use tracing::{info, warn};

use workforce_agent::llm::failover::{FailoverConfig, FailoverProvider, RetryConfig};
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
use super::prompt::{build_lookup_prompt, build_prompt, PromptInput};
use super::sources::SourceIndex;

const DEFAULT_TIER_KEY: &str = "anthropic";
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
        /// Where the applied changes' evidence stands (`path lines a–b`),
        /// for the reviewer.
        citations: Vec<String>,
    },
    /// The model judged the page accurate as it stands.
    Unchanged { reason: String },
}

/// Curate one page with the configured tiers: resolve the editor and the
/// verifier, then give the editor the sources (listed for lookup when they
/// overflow the context budget) and let it change the page, each change
/// checked by the verifier against the sources it cites.
#[allow(clippy::too_many_arguments)]
pub async fn curate_page(
    page: &str,
    tier_spec: &str,
    verifier_tier: &str,
    limits: &CuratorConfig,
    system_prompt: &str,
    input: &PromptInput,
    schema: TomlSchema,
    sources: &SourceIndex,
    vault: Option<&Arc<Vault>>,
) -> Result<Curation> {
    let (provider, model) = provider_for(tier_spec, vault).await?;
    let (verifier_provider, verifier_model) = provider_for(verifier_tier, vault).await?;
    if verifier_model == model {
        warn!(
            %model,
            "the verifier runs on the editor's model; a different model catches more"
        );
    }
    let verifier = Verifier {
        provider: &*verifier_provider,
        model: verifier_model,
        max_tokens: limits.max_tokens,
    };
    curate_with(
        &*provider,
        &model,
        page,
        limits,
        system_prompt,
        input,
        schema,
        sources,
        Some(&verifier),
    )
    .await
}

/// The provider for a tier, behind the hub's own retry policy: transient
/// errors are retried with backoff, and a rate limit that names when to come
/// back (OpenRouter: "could not verify available credits in time", with
/// `Retry-After`) is waited out on the same model. No failover chain: the
/// curator's models are chosen, not interchangeable.
async fn provider_for(
    tier_spec: &str,
    vault: Option<&Arc<Vault>>,
) -> Result<(Box<dyn LlmProvider>, String)> {
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
    Ok((Box::new(with_retries(Arc::from(provider))), model))
}

fn with_retries(provider: Arc<dyn LlmProvider>) -> FailoverProvider {
    FailoverProvider::new(
        provider,
        FailoverConfig {
            retry: RetryConfig {
                retry_rate_limited: true,
                ..RetryConfig::default()
            },
            ..FailoverConfig::default()
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn curate_with(
    provider: &dyn LlmProvider,
    model: &str,
    page: &str,
    limits: &CuratorConfig,
    system_prompt: &str,
    input: &PromptInput,
    schema: TomlSchema,
    sources: &SourceIndex,
    verifier: Option<&Verifier<'_>>,
) -> Result<Curation> {
    let single_pass = build_prompt(input);
    let estimated =
        estimate_tokens(system_prompt) + estimate_tokens(&single_pass) + limits.max_tokens as usize;
    let user_prompt = if estimated <= limits.context_safety_tokens as usize {
        info!(page, %model, estimated, "curator: sources included");
        single_pass
    } else {
        info!(
            page,
            %model,
            estimated,
            threshold = limits.context_safety_tokens,
            "curator: sources too large to include; listed for lookup"
        );
        build_lookup_prompt(input)
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
        sources,
        verifier,
    )
    .await
    .wrap_err_with(|| context_overflow_hint(page))
}

// ─── Editing the page ──────────────────────────────────────────────────────

const CUT_OFF_NOTE: &str = "Your last reply was cut off before it ended, so none of it was \
     applied. Make smaller changes: one passage per edit call, and fewer calls per reply.";
const TOOLS_ONLY_NOTE: &str = "Reply with tool calls only: `search_sources` or `read_source` \
     to look a fact up, `edit` or `append` to change the page, then `finish`; or `keep_page` if \
     it needs no change.";

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

/// What an edit is checked against beyond its own text.
pub struct EditContext<'a> {
    gate: ExampleGate,
    sources: &'a SourceIndex,
    /// `None` only in tests of the loop itself: `curate_page` always sets it.
    verifier: Option<&'a Verifier<'a>>,
    /// The cited ranges of the changes applied so far.
    citations: std::sync::Mutex<Vec<String>>,
}

/// Checks one change against the sources it cites. A second model sees the
/// passage before and after the change and only the cited excerpts, and
/// lists each statement the change adds that they don't support. It runs
/// on a different model from the editor, so the two don't share blind
/// spots.
pub struct Verifier<'a> {
    pub provider: &'a dyn LlmProvider,
    pub model: String,
    pub max_tokens: u32,
}

const VERIFIER_PROMPT: &str = "You check edits to end-user documentation against the \
     source code they cite. You see the passage before the edit, the passage after it, and \
     excerpts from the cited sources. List every statement the edit adds or changes that the \
     excerpts don't support: a command, subcommand, flag, argument, key, section, default, \
     value, name, limit or behaviour that the excerpts don't show, or show differently. Also \
     list anything the edit removes or weakens that the excerpts show is still true (for \
     example, dropping that an action needs an admin). Text identical in both passages is not \
     yours to judge, and neither are wording, tone or formatting. Answer by calling `verdict` \
     once; an empty list means the edit stands on the excerpts.";

impl Verifier<'_> {
    /// The statements in `after` that `excerpts` don't support.
    async fn unsupported(
        &self,
        before: &str,
        after: &str,
        excerpts: &[String],
    ) -> Result<Vec<String>> {
        let prompt = format!(
            "## Passage before the edit\n<<<\n{before}\n>>>\n\n\
             ## Passage after the edit\n<<<\n{after}\n>>>\n\n\
             ## Excerpts from the cited sources\n{}",
            excerpts.join("\n")
        );
        let tools = [LlmToolDef {
            name: "verdict".to_string(),
            description: "Report the statements the excerpts don't support; empty when all are."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"unsupported": {"type": "array", "items": {"type": "string"}}},
                "required": ["unsupported"],
            }),
        }];
        let messages = [user_text(prompt)];
        for _ in 0..2 {
            let response = invoke(
                self.provider,
                &self.model,
                Some(VERIFIER_PROMPT),
                &messages,
                &tools,
                self.max_tokens,
            )
            .await?;
            if is_cut_off(&response, self.max_tokens) {
                continue;
            }
            let verdict = response
                .tool_uses()
                .into_iter()
                .find(|(_, name, _)| *name == "verdict")
                .and_then(|(_, _, input)| {
                    input.get("unsupported").and_then(Value::as_array).cloned()
                });
            if let Some(list) = verdict {
                return Ok(list
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect());
            }
        }
        Err(eyre!("the verifier gave no verdict; nothing was applied"))
    }
}

#[allow(clippy::too_many_arguments)]
async fn edit_page(
    provider: &dyn LlmProvider,
    model: &str,
    page: &str,
    limits: &CuratorConfig,
    system_prompt: &str,
    user_prompt: &str,
    current: Option<&str>,
    schema: TomlSchema,
    sources: &SourceIndex,
    verifier: Option<&Verifier<'_>>,
) -> Result<Curation> {
    let tools = edit_tools();
    let context = EditContext {
        gate: ExampleGate::new(schema, current),
        sources,
        verifier,
        citations: std::sync::Mutex::new(Vec::new()),
    };
    let mut draft = PageDraft::new(current);
    let mut messages = vec![user_text(user_prompt.to_string())];
    if let Some(note) = context.gate.baseline_note() {
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
            info!(
                page,
                turn, "curator: reply without tool calls; asked for tools"
            );
            push_user_text(&mut messages, TOOLS_ONLY_NOTE);
            continue;
        }

        let (results, verdict) = apply_calls(&mut draft, &calls, &context).await?;
        info!(
            page,
            turn,
            calls = ?calls.iter().map(|c| c.1.as_str()).collect::<Vec<_>>(),
            refused = ?results
                .iter()
                .filter_map(|r| r.1.as_ref().err().map(|e| e.chars().take(120).collect::<String>()))
                .collect::<Vec<_>>(),
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
                let (_, still) = context.gate.split(draft.text());
                let mut citations = context
                    .citations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                citations.sort();
                citations.dedup();
                return Ok(Curation::Edited {
                    citations,
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

/// Apply one reply's tool calls in order. Lookups answer without touching
/// the page. A change applies only when its passage matches, its evidence
/// is in the sources word for word, and the verifier finds nothing in it
/// the evidence doesn't support. `finish` and `keep_page` are judged after
/// the rest, and stand only when no change in the reply was refused;
/// `finish` also needs the page's examples to pass their check.
async fn apply_calls(
    draft: &mut PageDraft,
    calls: &[(String, String, Value)],
    context: &EditContext<'_>,
) -> Result<(Vec<(String, Result<String, String>)>, Option<Verdict>)> {
    let mut results: BTreeMap<usize, (String, Result<String, String>)> = BTreeMap::new();
    let mut verdicts = Vec::new();
    let mut refused_change = false;
    for (index, (id, name, input)) in calls.iter().enumerate() {
        let result = match name.as_str() {
            "read_source" => match string_args(input, &["path"]) {
                Ok(args) => context.sources.read(
                    &args[0],
                    line_arg(input, "start_line"),
                    line_arg(input, "end_line"),
                ),
                Err(e) => Err(e),
            },
            "search_sources" => match string_args(input, &["pattern"]) {
                Ok(args) => {
                    let path = input.get("path").and_then(Value::as_str);
                    context.sources.search(&args[0], path).await
                }
                Err(e) => Err(e),
            },
            "edit" => {
                let result = edit_call(draft, input, context).await?;
                refused_change |= result.is_err();
                result
            }
            "append" => {
                let result = append_call(draft, input, context).await?;
                refused_change |= result.is_err();
                result
            }
            "finish" | "keep_page" => {
                verdicts.push(index);
                continue;
            }
            other => Err(format!(
                "unknown tool `{other}`; use read_source, search_sources, edit, append, finish \
                 or keep_page"
            )),
        };
        results.insert(index, (id.clone(), result));
    }

    let mut verdict = None;
    for index in verdicts {
        let (id, name, input) = &calls[index];
        let result = if verdict.is_some() {
            Err("this reply already ended the edit".to_string())
        } else if refused_change {
            Err("a change in this reply was refused; fix it before ending the edit".to_string())
        } else if name == "finish" {
            string_args(input, &["summary"]).and_then(|args| {
                if !draft.is_changed() {
                    return Err(
                        "the page is unchanged; call keep_page if it needs no change".to_string(),
                    );
                }
                let (new, _) = context.gate.split(draft.text());
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
    Ok((results.into_values().collect(), verdict))
}

async fn edit_call(
    draft: &mut PageDraft,
    input: &Value,
    context: &EditContext<'_>,
) -> Result<Result<String, String>> {
    let args = match string_args(input, &["old_text", "new_text"]) {
        Ok(args) => args,
        Err(e) => return Ok(Err(e)),
    };
    if let Err(e) = draft.locate(&args[0]) {
        return Ok(Err(e.to_string()));
    }
    let cited = match vouched_for(&args[0], &args[1], input, context).await? {
        Ok(cited) => cited,
        Err(refusal) => return Ok(Err(refusal)),
    };
    if let Err(e) = draft.replace(&args[0], &args[1]) {
        return Ok(Err(e.to_string()));
    }
    record(context, cited);
    Ok(Ok("applied".to_string()))
}

async fn append_call(
    draft: &mut PageDraft,
    input: &Value,
    context: &EditContext<'_>,
) -> Result<Result<String, String>> {
    let args = match string_args(input, &["text"]) {
        Ok(args) => args,
        Err(e) => return Ok(Err(e)),
    };
    let cited = match vouched_for("", &args[0], input, context).await? {
        Ok(cited) => cited,
        Err(refusal) => return Ok(Err(refusal)),
    };
    draft.append(&args[0]);
    record(context, cited);
    Ok(Ok("appended".to_string()))
}

/// The cited ranges of an applied change, logged and kept for the PR.
fn record(context: &EditContext<'_>, cited: Vec<String>) {
    info!(cited = ?cited, "curator: change applied");
    context
        .citations
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(cited);
}

/// Whether the change from `before` to `after` stands on its evidence: every
/// cited quote is in its source word for word, and the verifier finds no
/// statement the excerpts don't support. `Ok` inside holds the cited ranges;
/// `Err` inside is the refusal the model sees; the outer error is a
/// verifier that couldn't be asked.
async fn vouched_for(
    before: &str,
    after: &str,
    input: &Value,
    context: &EditContext<'_>,
) -> Result<Result<Vec<String>, String>> {
    let excerpts = match cited_excerpts(input, context.sources) {
        Ok(excerpts) => excerpts,
        Err(e) => return Ok(Err(e)),
    };
    let ranges: Vec<String> = excerpts
        .iter()
        .filter_map(|e| e.lines().next())
        .map(|header| header.trim_end_matches(':').to_string())
        .collect();
    let Some(verifier) = context.verifier else {
        return Ok(Ok(ranges));
    };
    let unsupported = verifier.unsupported(before, after, &excerpts).await?;
    if unsupported.is_empty() {
        return Ok(Ok(ranges));
    }
    let list: Vec<String> = unsupported.iter().map(|u| format!("- {u}")).collect();
    Ok(Err(format!(
        "refused: the sources you cited don't support:\n{}\nLook these up with \
         search_sources / read_source and cite what supports them, or leave them out.",
        list.join("\n")
    )))
}

/// The excerpt around each cited quote, or why a citation doesn't hold.
fn cited_excerpts(input: &Value, sources: &SourceIndex) -> Result<Vec<String>, String> {
    let items = input
        .get("evidence")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(|| {
            "missing `evidence`: cite at least one source the change rests on, as \
             {\"path\": …, \"quote\": …} with the quote copied from that source"
                .to_string()
        })?;
    items
        .iter()
        .map(|item| {
            let path = item.get("path").and_then(Value::as_str);
            let quote = item.get("quote").and_then(Value::as_str);
            match (path, quote) {
                (Some(path), Some(quote)) => sources.cited(path, quote),
                _ => Err("each evidence item needs a `path` and a `quote`".to_string()),
            }
        })
        .collect()
}

fn line_arg(input: &Value, name: &str) -> Option<usize> {
    input
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
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
    let evidence = json!({
        "type": "array",
        "minItems": 1,
        "description": "The sources the change rests on: each a repo-relative `path` (or a \
            generated source's name) and a `quote` copied word for word from it. A verifier \
            checks the change against these excerpts alone.",
        "items": {
            "type": "object",
            "properties": {"path": {"type": "string"}, "quote": {"type": "string"}},
            "required": ["path", "quote"],
        },
    });
    vec![
        tool(
            "search_sources",
            "Search the repository's files and the generated sources for lines matching a \
             regular expression. Look a fact up before you write it.",
            json!({
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "Limit to paths matching this git pathspec, e.g. `crates/cli/**`."},
            }),
            &["pattern"],
        ),
        tool(
            "read_source",
            "Read numbered lines of a repository file or a generated source, up to 300 at a \
             time.",
            json!({
                "path": {"type": "string"},
                "start_line": {"type": "integer"},
                "end_line": {"type": "integer"},
            }),
            &["path"],
        ),
        tool(
            "edit",
            "Replace one passage of the page. `old_text` is copied exactly from the page as \
             it stands now (your earlier edits included) and must occur exactly once; quote \
             enough surrounding text to make it unique. `new_text` replaces it; empty deletes \
             it. Keep each edit to one passage, and cite the evidence for what it says.",
            json!({
                "old_text": {"type": "string"},
                "new_text": {"type": "string"},
                "evidence": evidence.clone(),
            }),
            &["old_text", "new_text", "evidence"],
        ),
        tool(
            "append",
            "Add text to the end of the page. Use it to write a page that doesn't exist yet, \
             a section at a time, citing the evidence for each.",
            json!({"text": {"type": "string"}, "evidence": evidence}),
            &["text", "evidence"],
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
    // The curator wants edits and verdicts, not a chain of thought: a model
    // that reasons by default would otherwise spend the budget on it.
    // Cached where the route supports it (Claude models): the edit loop
    // resends the sources every turn.
    with_reasoning_suppressed(provider.create_message(
        system_prompt,
        messages,
        tools,
        Some(model),
        Some(max_tokens),
        true,
    ))
    .await
}

fn context_overflow_hint(page: &str) -> String {
    format!(
        "curating page `{page}` failed \
         (if the underlying error mentions context length, lower \
         `[curator] context_safety_tokens` so large sources are listed for \
         lookup instead of included, or scope this page's sources in \
         docs-watch.toml)"
    )
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

    /// The source the loop tests cite.
    const SPEC: &str = "The curator documents its flags, shortcuts and commands.\nIt prints the plan with --dry-run.\n";

    fn spec_sources() -> SourceIndex {
        SourceIndex::new(
            std::path::Path::new("/nonexistent"),
            vec![("spec".to_string(), SPEC.to_string())],
        )
    }

    /// Tool calls as a reply. A change without `evidence` cites the spec;
    /// `"evidence": null` sends it with none.
    fn calls(list: &[(&str, Value)]) -> LlmResponse {
        reply(
            list.iter()
                .enumerate()
                .map(|(i, (name, input))| {
                    let mut input = input.clone();
                    if matches!(*name, "edit" | "append") {
                        let object = input.as_object_mut().unwrap();
                        match object.get("evidence") {
                            None => {
                                object.insert(
                                    "evidence".into(),
                                    json!([{"path": "spec", "quote": "documents its flags"}]),
                                );
                            }
                            Some(Value::Null) => {
                                object.remove("evidence");
                            }
                            Some(_) => {}
                        }
                    }
                    ContentBlock::tool_use(format!("c{i}"), name.to_string(), input)
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
        run_checked(provider, None, current).await
    }

    async fn run_checked(
        provider: &Scripted,
        verifier: Option<&Scripted>,
        current: Option<&str>,
    ) -> Result<Curation> {
        let sources = spec_sources();
        let verifier = verifier.map(|v| Verifier {
            provider: v,
            model: "verifier".into(),
            max_tokens: 2_000,
        });
        edit_page(
            provider,
            "m",
            "cli.mdx",
            &limits(),
            "system",
            "curate",
            current,
            TomlSchema::Workforce,
            &sources,
            verifier.as_ref(),
        )
        .await
    }

    fn verdict(unsupported: &[&str]) -> LlmResponse {
        calls(&[("verdict", json!({ "unsupported": unsupported }))])
    }

    // ── Retries ────────────────────────────────────────────────────────────

    /// Refuses its first call the way OpenRouter does when it can't check
    /// credits in time, then answers.
    struct RateLimitedOnce {
        calls: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for RateLimitedOnce {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls == 1 {
                return Err(eyre!(
                    "OpenRouter API error (429 Too Many Requests): {{\"error\":{{\"message\":\"OpenRouter could not verify available credits for this request in time. Retry shortly.\"}}}} (retry-after: 0s)"
                ));
            }
            Ok(reply(
                vec![ContentBlock::Text { text: "ok".into() }],
                StopReason::EndTurn,
                5,
            ))
        }
    }

    #[tokio::test]
    async fn a_rate_limited_call_is_retried_after_its_retry_after() {
        let inner = Arc::new(RateLimitedOnce {
            calls: Mutex::new(0),
        });
        let provider = with_retries(inner.clone());
        let messages = [user_text("hi".into())];
        let response = invoke(
            &provider,
            "deepseek/deepseek-v4-pro",
            None,
            &messages,
            &[],
            100,
        )
        .await
        .expect("the 429 is waited out, not returned");
        assert_eq!(response.text().as_deref(), Some("ok"));
        assert_eq!(*inner.calls.lock().unwrap(), 2);
    }

    // ── Evidence and the verifier ──────────────────────────────────────────

    #[tokio::test]
    async fn a_change_without_evidence_is_refused() {
        let bare = (
            "edit",
            json!({"old_text": "`--all`", "new_text": "`--every`", "evidence": null}),
        );
        let provider = Scripted::new(vec![calls(&[bare]), calls(&[add_dry_run(), finish()])]);
        let Curation::Edited { text, .. } = run(&provider, Some(PAGE)).await.unwrap() else {
            panic!("expected an edit");
        };
        assert!(text.contains("`--all`"), "an uncited edit was applied");
        let sent = serde_json::to_string(&provider.seen.lock().unwrap()[1]).unwrap();
        assert!(sent.contains("missing `evidence`"), "{sent}");
    }

    #[tokio::test]
    async fn evidence_not_in_its_source_is_refused() {
        let invented = (
            "edit",
            json!({"old_text": "`--all`", "new_text": "`--every`",
                   "evidence": [{"path": "spec", "quote": "the flag is called --every"}]}),
        );
        let provider = Scripted::new(vec![calls(&[invented]), calls(&[add_dry_run(), finish()])]);
        let Curation::Edited { text, .. } = run(&provider, Some(PAGE)).await.unwrap() else {
            panic!("expected an edit");
        };
        assert!(text.contains("`--all`"));
        let sent = serde_json::to_string(&provider.seen.lock().unwrap()[1]).unwrap();
        assert!(sent.contains("doesn't contain the quote"), "{sent}");
    }

    #[tokio::test]
    async fn the_verifier_refuses_what_the_evidence_does_not_support() {
        let provider = Scripted::new(vec![
            calls(&[add_dry_run(), finish()]),
            calls(&[add_dry_run(), finish()]),
        ]);
        let verifier = Scripted::new(vec![
            verdict(&["that `--dry-run` prints the plan"]),
            verdict(&[]),
        ]);
        let Curation::Edited { text, edits, .. } =
            run_checked(&provider, Some(&verifier), Some(PAGE))
                .await
                .unwrap()
        else {
            panic!("expected an edit");
        };
        assert_eq!(edits, 1, "the refused edit was applied");
        assert!(text.contains("--dry-run"));
        let refusal = serde_json::to_string(&provider.seen.lock().unwrap()[1]).unwrap();
        assert!(refusal.contains("don't support"), "{refusal}");
        assert!(refusal.contains("fix it before ending"), "{refusal}");
        // The verifier saw the change and the cited excerpt.
        let asked = serde_json::to_string(&verifier.seen.lock().unwrap()[0]).unwrap();
        assert!(asked.contains("Passage after the edit"), "{asked}");
        assert!(asked.contains("documents its flags"), "{asked}");
    }

    #[tokio::test]
    async fn a_verifier_that_gives_no_verdict_stops_the_page() {
        let provider = Scripted::new(vec![calls(&[add_dry_run(), finish()])]);
        let prose = || {
            reply(
                vec![ContentBlock::Text {
                    text: "Looks fine.".into(),
                }],
                StopReason::EndTurn,
                10,
            )
        };
        let verifier = Scripted::new(vec![prose(), prose()]);
        let err = run_checked(&provider, Some(&verifier), Some(PAGE))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no verdict"), "{err}");
    }

    #[tokio::test]
    async fn lookups_answer_without_changing_the_page() {
        let provider = Scripted::new(vec![
            calls(&[
                ("read_source", json!({"path": "spec", "start_line": 2})),
                ("read_source", json!({"path": "../secrets"})),
            ]),
            calls(&[("keep_page", json!({"reason": "accurate"}))]),
        ]);
        let outcome = run(&provider, Some(PAGE)).await.unwrap();
        assert_eq!(
            outcome,
            Curation::Unchanged {
                reason: "accurate".into()
            }
        );
        let answers = serde_json::to_string(&provider.seen.lock().unwrap()[1]).unwrap();
        assert!(
            answers.contains("It prints the plan with --dry-run."),
            "{answers}"
        );
        assert!(
            answers.contains("outside the sources you may read"),
            "{answers}"
        );
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
        let Curation::Edited { citations, .. } = run(
            &Scripted::new(vec![calls(&[add_dry_run(), finish()])]),
            Some(PAGE),
        )
        .await
        .unwrap() else {
            unreachable!()
        };
        assert_eq!(citations, vec!["spec lines 1–2".to_string()]);
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

    #[test]
    fn estimate_tokens_uses_chars_over_3() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 2);
        assert_eq!(estimate_tokens("a".repeat(300).as_str()), 100);
    }
}
