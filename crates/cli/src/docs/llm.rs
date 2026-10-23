//! LLM dispatch for the curator. Single-pass when the prompt fits the
//! configured context budget; chunked map-reduce when it doesn't.
//!
//! Map step (overflow only): split sources into chunks, ask the LLM to
//! extract page-relevant notes from each chunk.
//! Reduce step: ask the LLM to write the page from combined notes.

use std::sync::Arc;

use eyre::{eyre, Context, Result};
use futures::future::try_join_all;
use tracing::{info, warn};

use workforce_agent::llm::provider::{
    create_provider_async, provider_env_key, provider_env_var_hint, LlmProvider, ProviderKind,
};
use workforce_agent::llm::tiers::TierRegistry;
use workforce_agent::llm::types::{ContentBlock, LlmMessage, LlmResponse, Role, StopReason};
use workforce_hub::vault::Vault;

use super::prompt::{
    build_notes_prompt, build_prompt, build_reduce_prompt, PromptInput, SourceFile,
};

const DEFAULT_TIER_KEY: &str = "anthropic";
const NOTES_MAX_TOKENS: u32 = 2_000;
const MAX_MAP_CHUNKS: usize = 10;
const OVERHEAD_TOKEN_RESERVE: u32 = 15_000;

#[derive(Debug)]
pub struct CuratorDraft {
    pub text: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub async fn draft_page(
    page: &str,
    tier_spec: &str,
    max_tokens: u32,
    context_safety_tokens: u32,
    system_prompt: &str,
    input: &PromptInput,
    vault: Option<&Arc<Vault>>,
) -> Result<CuratorDraft> {
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

    let single_pass = build_prompt(input);
    let estimated =
        estimate_tokens(system_prompt) + estimate_tokens(&single_pass) + max_tokens as usize;
    if estimated <= context_safety_tokens as usize {
        info!(page, provider = ?kind, %model, estimated, "curator: single-pass");
        return single_pass_draft(
            page,
            &single_pass,
            system_prompt,
            max_tokens,
            &*provider,
            &model,
        )
        .await;
    }

    info!(
        page,
        provider = ?kind,
        %model,
        estimated,
        threshold = context_safety_tokens,
        "curator: multi-pass (input exceeds budget)"
    );
    multi_pass_draft(
        page,
        input,
        system_prompt,
        max_tokens,
        context_safety_tokens,
        &*provider,
        &model,
    )
    .await
}

async fn single_pass_draft(
    page: &str,
    user_prompt: &str,
    system_prompt: &str,
    max_tokens: u32,
    provider: &dyn LlmProvider,
    model: &str,
) -> Result<CuratorDraft> {
    let response = invoke(
        provider,
        model,
        Some(system_prompt),
        user_prompt,
        max_tokens,
    )
    .await
    .wrap_err_with(|| context_overflow_hint(page))?;
    response_into_draft(response)
}

async fn multi_pass_draft(
    page: &str,
    input: &PromptInput,
    system_prompt: &str,
    max_tokens: u32,
    context_safety_tokens: u32,
    provider: &dyn LlmProvider,
    model: &str,
) -> Result<CuratorDraft> {
    let chunk_budget_chars =
        (context_safety_tokens.saturating_sub(OVERHEAD_TOKEN_RESERVE)) as usize * 3;
    let chunks = split_into_chunks(&input.sources, chunk_budget_chars);
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

    let map_calls = chunks.iter().enumerate().map(|(i, chunk)| {
        let label = format!("{page} [chunk {}/{}]", i + 1, chunks.len());
        extract_notes(label, chunk, provider, model)
    });
    let notes_parts: Vec<String> = try_join_all(map_calls).await?;
    let combined_notes = notes_parts.join("\n\n");

    info!(
        page,
        notes_chars = combined_notes.len(),
        "curator: reducing notes into final page"
    );
    let reduce_prompt = build_reduce_prompt(input, &combined_notes);
    let response = invoke(
        provider,
        model,
        Some(system_prompt),
        &reduce_prompt,
        max_tokens,
    )
    .await
    .wrap_err_with(|| context_overflow_hint(page))?;
    response_into_draft(response)
}

async fn extract_notes(
    label: String,
    chunk: &[SourceFile],
    provider: &dyn LlmProvider,
    model: &str,
) -> Result<String> {
    let page = label.split_whitespace().next().unwrap_or("");
    let prompt = build_notes_prompt(page, chunk);
    let response = invoke(provider, model, None, &prompt, NOTES_MAX_TOKENS)
        .await
        .wrap_err_with(|| format!("notes extraction failed for `{label}`"))?;
    response
        .text()
        .ok_or_else(|| eyre!("notes call for `{label}` returned no text content"))
}

async fn invoke(
    provider: &dyn LlmProvider,
    model: &str,
    system_prompt: Option<&str>,
    user_prompt: &str,
    max_tokens: u32,
) -> Result<LlmResponse> {
    let messages = vec![LlmMessage {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: user_prompt.to_string(),
        }],
    }];
    provider
        .create_message(
            system_prompt,
            &messages,
            &[],
            Some(model),
            Some(max_tokens),
            false,
        )
        .await
}

fn response_into_draft(response: LlmResponse) -> Result<CuratorDraft> {
    if matches!(response.stop_reason, StopReason::MaxTokens) {
        warn!(
            output_tokens = response.usage.output_tokens,
            "curator: response hit max_tokens; output may be truncated"
        );
    }
    let text = response
        .text()
        .ok_or_else(|| eyre!("LLM returned no text content"))?;
    Ok(CuratorDraft {
        text,
        model: response.model,
        input_tokens: response.usage.input_tokens as u64,
        output_tokens: response.usage.output_tokens as u64,
    })
}

fn context_overflow_hint(page: &str) -> String {
    format!(
        "LLM call failed for page `{page}` \
         (if the underlying error mentions context length, lower \
         `[curator] context_safety_tokens` so multi-pass triggers sooner, \
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

/// Sentinel the curator emits when a page needs no change. See the
/// "Decide whether the page needs updating" section of the system prompt.
const KEEP_EXISTING_MARKER: &str = "KEEP_EXISTING";

/// If `raw` is the keep-existing sentinel, return its reason clause (empty
/// when the model omitted one). A real page always starts with `---`
/// frontmatter, so this never matches genuine output.
pub fn keep_existing_reason(raw: &str) -> Option<String> {
    let rest = raw.trim().strip_prefix(KEEP_EXISTING_MARKER)?;
    Some(rest.trim_start_matches([':', ' ']).trim().to_string())
}

/// Strip an outer ` ```mdx ... ``` ` fence if the model wrapped the
/// page in one. Internal code blocks are preserved because the strip
/// only fires when the response itself *starts* with a fence. Any
/// other preamble (`Here's the page:`) is returned verbatim — the
/// post-edit verify step catches the resulting bad frontmatter.
pub fn extract_mdx(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.starts_with("```") {
        if let Some(content_start) = trimmed.find('\n') {
            let inner_start = content_start + 1;
            if let Some(end_rel) = trimmed[inner_start..].rfind("\n```") {
                return trimmed[inner_start..inner_start + end_rel].trim_end();
            }
        }
    }
    trimmed
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
    fn extract_mdx_strips_outer_fence_when_response_starts_with_one() {
        let raw = "```mdx\n---\ntitle: X\n---\n\n# Hello\n```\n";
        let out = extract_mdx(raw);
        assert_eq!(out, "---\ntitle: X\n---\n\n# Hello");
    }

    #[test]
    fn extract_mdx_handles_no_language_fence() {
        let raw = "```\n---\ntitle: Z\n---\n```";
        let out = extract_mdx(raw);
        assert_eq!(out, "---\ntitle: Z\n---");
    }

    #[test]
    fn keep_existing_reason_parses_marker_with_reason() {
        assert_eq!(
            keep_existing_reason("KEEP_EXISTING: sources unchanged since last curation"),
            Some("sources unchanged since last curation".to_string())
        );
    }

    #[test]
    fn keep_existing_reason_accepts_bare_marker() {
        assert_eq!(
            keep_existing_reason("  KEEP_EXISTING  "),
            Some(String::new())
        );
    }

    #[test]
    fn keep_existing_reason_ignores_real_page() {
        assert!(keep_existing_reason("---\ntitle: X\n---\n\n# Body").is_none());
    }

    #[test]
    fn extract_mdx_returns_raw_mdx_unchanged() {
        let raw = "---\ntitle: A\n---\n\n# Body\n";
        assert_eq!(extract_mdx(raw), "---\ntitle: A\n---\n\n# Body");
    }

    #[test]
    fn extract_mdx_passes_preamble_through_to_verify() {
        let raw = "Here is the page:\n\n---\ntitle: X\n---\n";
        let out = extract_mdx(raw);
        assert!(
            out.starts_with("Here is"),
            "preamble must pass through verbatim so verify rejects the file: {out}",
        );
    }

    #[test]
    fn extract_mdx_preserves_internal_code_blocks() {
        let raw =
            "---\ntitle: T\n---\n\n## Install\n\n```sh\nworkforce serve\n```\n\n## Next\n\nMore prose.\n";
        let out = extract_mdx(raw);
        assert!(out.contains("workforce serve"), "lost code block: {out}");
        assert!(out.contains("## Next"), "truncated body: {out}");
        assert!(out.contains("More prose"), "truncated tail: {out}");
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
