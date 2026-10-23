//! code.search - Full-text codebase search using ripgrep.

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::process::Command;
use workforce_agent::tools::{Tool, ToolResult};

const MAX_RESULTS: usize = 100;
const DEFAULT_CONTEXT_LINES: usize = 2;
const MAX_MATCH_CHARS: usize = 200;

/// Hard cap on serialised output (bytes). At ~4 bytes/token this keeps the
/// worst case under ~8K tokens — incident 2026-05-18 had a single call
/// return ~498K tokens and trigger nuclear compaction.
const MAX_TOTAL_BYTES: usize = 32 * 1024;

/// Pre-serialisation budget leaving headroom for JSON punctuation and the
/// `ToolResult` envelope.
const MAX_CONTENT_BYTES: usize = 24 * 1024;

/// Always-on ignore globs. Secret material must come via `vault.*` tools,
/// never via grep. Defence-in-depth alongside the tool-result tokeniser.
const IGNORE_GLOBS: &[&str] = &[
    "!vault*",
    "!*credentials*",
    "!*.enc",
    "!.env",
    "!.env.*",
    "!*.pem",
    "!*.key",
    "!id_rsa*",
    "!id_ed25519*",
    "!*.pfx",
    "!*.p12",
    "!secrets.*",
    "!*-audit.jsonl",
];

/// Truncate `s` at a UTF-8 char boundary, returning `(prefix, was_truncated)`.
fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_string(), false);
    }
    let mut out = String::with_capacity(max);
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    (out, true)
}

/// code.search tool - Full-text search with ripgrep.
pub struct CodeSearchTool {
    allowed_paths: Vec<PathBuf>,
}

impl CodeSearchTool {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self { allowed_paths }
    }

    async fn search(
        &self,
        pattern: &str,
        path: Option<&PathBuf>,
        file_type: Option<&str>,
        case_sensitive: bool,
        context_lines: usize,
        offset: usize,
        max_results: usize,
    ) -> Result<SearchOutput> {
        if Command::new("rg").arg("--version").output().await.is_err() {
            return Err(eyre!(
                "ripgrep (rg) not found. Please install ripgrep: https://github.com/BurntSushi/ripgrep"
            ));
        }

        let search_path = match path {
            Some(p) => p.clone(),
            None => self
                .allowed_paths
                .first()
                .ok_or_else(|| eyre!("No workspace configured"))?
                .clone(),
        };

        // Probe one match past the page so we can set `next_offset` accurately.
        let probe_limit = offset.saturating_add(max_results).saturating_add(1);

        let mut cmd = Command::new("rg");
        // Kill the child if our future is dropped (soft-deadline timeout or a
        // hard-stop abort). Without this, an interrupted/timed-out search leaves
        // an orphaned ripgrep chewing CPU/disk — a known driftwerk-prod pressure.
        cmd.kill_on_drop(true);
        cmd.arg("--json")
            .arg("--line-number")
            .arg("--no-messages")
            .arg("--max-count")
            .arg(probe_limit.to_string());

        for glob in IGNORE_GLOBS {
            cmd.arg("--glob").arg(glob);
        }
        if context_lines > 0 {
            cmd.arg("--context").arg(context_lines.to_string());
        }
        if !case_sensitive {
            cmd.arg("--ignore-case");
        }
        if let Some(ft) = file_type {
            cmd.arg("--type").arg(ft);
        }
        cmd.arg(pattern).arg(&search_path);

        let output = cmd.output().await?;
        let stdout = String::from_utf8_lossy(&output.stdout);

        let all_matches = parse_rg_stream(&stdout, probe_limit);
        let scanned = all_matches.len();
        let page_cap = offset.saturating_add(max_results);
        let has_more = scanned > page_cap;

        let page: Vec<SearchMatch> = all_matches
            .into_iter()
            .skip(offset)
            .take(max_results)
            .collect();
        let (matches, budget_cut) = apply_byte_budget(page, MAX_CONTENT_BYTES);

        let returned = matches.len();
        let truncated = budget_cut;
        let truncation_reason = budget_cut.then(|| {
            format!(
                "result body exceeded {MAX_CONTENT_BYTES} byte budget; \
                 narrow the `pattern` / `path` / `file_type` or page via `offset`"
            )
        });
        let next_offset = (budget_cut || has_more).then(|| offset.saturating_add(returned));
        let searched_files = matches
            .iter()
            .map(|m| m.path.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len();

        Ok(SearchOutput {
            matches_returned: returned,
            searched_files,
            next_offset,
            truncated,
            truncation_reason,
            matches,
        })
    }
}

/// Parse ripgrep's `--json` stream into [`SearchMatch`] rows with per-match
/// character caps. Stops at `limit` matches.
fn parse_rg_stream(stdout: &str, limit: usize) -> Vec<SearchMatch> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let json: serde_json::Value = serde_json::from_str(line).ok()?;
            if json["type"] != "match" {
                return None;
            }
            let data = &json["data"];
            let path = data["path"]["text"].as_str().unwrap_or("").to_string();
            if workforce_agent::tools::paths::is_withheld(Path::new(&path)) {
                return None;
            }
            let line_no = data["line_number"].as_u64().unwrap_or(0) as usize;
            let raw = data["lines"]["text"].as_str().unwrap_or("").trim();
            let (match_text, _) = truncate_chars(raw, MAX_MATCH_CHARS);
            Some(SearchMatch {
                path,
                line: line_no,
                match_text,
                context_before: Vec::new(),
                context_after: Vec::new(),
            })
        })
        .take(limit)
        .collect()
}

fn match_byte_estimate(m: &SearchMatch) -> usize {
    const ROW_OVERHEAD: usize = 80;
    m.path.len()
        + m.match_text.len()
        + m.context_before.iter().map(String::len).sum::<usize>()
        + m.context_after.iter().map(String::len).sum::<usize>()
        + ROW_OVERHEAD
}

/// Drop matches from the tail until the running estimate fits the budget.
/// O(n) — single pass with a running total.
fn apply_byte_budget(matches: Vec<SearchMatch>, budget: usize) -> (Vec<SearchMatch>, bool) {
    let original_len = matches.len();
    let mut total = 0usize;
    let kept: Vec<SearchMatch> = matches
        .into_iter()
        .take_while(|m| {
            total += match_byte_estimate(m);
            total <= budget
        })
        .collect();
    let cut = kept.len() < original_len;
    (kept, cut)
}

#[derive(Deserialize)]
struct CodeSearchInput {
    pattern: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    file_type: Option<String>,
    #[serde(default = "default_case_sensitive")]
    case_sensitive: bool,
    #[serde(default = "default_context_lines")]
    context_lines: usize,
    #[serde(default = "default_max_results")]
    max_results: usize,
    #[serde(default)]
    offset: usize,
}

fn default_case_sensitive() -> bool {
    true
}

fn default_context_lines() -> usize {
    DEFAULT_CONTEXT_LINES
}

fn default_max_results() -> usize {
    MAX_RESULTS
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct SearchMatch {
    path: String,
    line: usize,
    #[serde(rename = "match")]
    match_text: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    context_before: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    context_after: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct SearchOutput {
    matches: Vec<SearchMatch>,
    matches_returned: usize,
    searched_files: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<usize>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncation_reason: Option<String>,
}

#[async_trait]
impl Tool for CodeSearchTool {
    fn name(&self) -> &str {
        "code.search"
    }

    fn soft_deadline(&self) -> Option<std::time::Duration> {
        // ripgrep on a sane tree finishes in well under a second. A multi-minute
        // run means it's traversing something huge (e.g. vendored build dirs in a
        // coding-session clone) — the 700s freeze that motivated this. Cap it well
        // under the runner's check-in interval and tell the model to narrow the
        // query rather than freezing the conversation.
        Some(std::time::Duration::from_secs(90))
    }

    fn description(&self) -> &str {
        "Full-text codebase search using ripgrep. Output is bounded: per-match \
         text truncated, total body capped, and pagination via `offset` / \
         `next_offset`. Files matching credential patterns (vault*, *credentials*, \
         *.pem, .env*, *.key, id_rsa*, etc.) are never returned — use `vault.*` \
         tools for secret material."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Search pattern (regex supported)"
                },
                "path": {
                    "type": "string",
                    "description": "Path to search (default: workspace root)"
                },
                "file_type": {
                    "type": "string",
                    "description": "File type filter (e.g., 'rust', 'ts', 'py', 'js', 'go')"
                },
                "case_sensitive": {
                    "type": "boolean",
                    "description": "Case-sensitive search (default: true)",
                    "default": true
                },
                "context_lines": {
                    "type": "integer",
                    "description": "Lines of context around matches (default: 2)",
                    "minimum": 0,
                    "default": DEFAULT_CONTEXT_LINES
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum results per page (default: 100)",
                    "minimum": 1,
                    "maximum": 1000
                },
                "offset": {
                    "type": "integer",
                    "description": "Pagination cursor — set to the previous response's `next_offset` to fetch the next page",
                    "minimum": 0,
                    "default": 0
                },
                "repo": {
                    "type": "string",
                    "description": super::utils::REPO_PARAM
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();

        let input: CodeSearchInput =
            serde_json::from_value(input).map_err(|e| eyre!("Invalid input: {e}"))?;

        let path = match &input.repo {
            Some(repo) => {
                let base = self
                    .allowed_paths
                    .first()
                    .ok_or_else(|| eyre!("No workspace configured"))?;
                match super::utils::resolve_repo_path(repo, base) {
                    Ok(p) => Some(p),
                    Err(e) => {
                        return Ok(ToolResult::failure(
                            format!("Search failed: {e}"),
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                }
            }
            None => input.path.as_ref().map(PathBuf::from),
        };

        let capped_max = input.max_results.clamp(1, 1000);

        match self
            .search(
                &input.pattern,
                path.as_ref(),
                input.file_type.as_deref(),
                input.case_sensitive,
                input.context_lines,
                input.offset,
                capped_max,
            )
            .await
        {
            Ok(output) => {
                let value = serde_json::to_value(output)?;
                let body_len = serde_json::to_vec(&value).map(|v| v.len()).unwrap_or(0);
                if body_len > MAX_TOTAL_BYTES {
                    return Ok(ToolResult::failure(
                        format!(
                            "Search result exceeded hard limit ({body_len} > {MAX_TOTAL_BYTES}); \
                             refine `pattern` / `path` / `file_type`"
                        ),
                        start.elapsed().as_millis() as u64,
                    ));
                }
                Ok(ToolResult::success(
                    value,
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Search failed: {e}"),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn has_ripgrep() -> bool {
        Command::new("rg").arg("--version").output().is_ok()
    }

    fn parse_output(value: serde_json::Value) -> SearchOutput {
        serde_json::from_value(value).expect("output is valid SearchOutput")
    }

    #[tokio::test]
    async fn test_search_simple_pattern() {
        if !has_ripgrep() {
            return;
        }
        let temp = TempDir::new().unwrap();
        fs::write(
            temp.path().join("test.rs"),
            "fn main() {\n    println!(\"hello\");\n}\nfn helper() {}\n",
        )
        .unwrap();

        let tool = CodeSearchTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "fn main" }))
            .await
            .unwrap();
        assert!(result.success);

        let output = parse_output(result.output);
        assert_eq!(output.matches_returned, 1);
        assert!(output.matches[0].path.contains("test.rs"));
        assert_eq!(output.matches[0].line, 1);
        assert!(output.next_offset.is_none());
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn test_search_case_insensitive() {
        if !has_ripgrep() {
            return;
        }
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("test.txt"), "Hello World\nhello world\n").unwrap();

        let tool = CodeSearchTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({
                "pattern": "HELLO",
                "case_sensitive": false
            }))
            .await
            .unwrap();
        assert!(result.success);

        let output = parse_output(result.output);
        assert_eq!(output.matches_returned, 2);
    }

    #[tokio::test]
    async fn test_search_with_file_type() {
        if !has_ripgrep() {
            return;
        }
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("test.rs"), "fn rust_func() {}").unwrap();
        fs::write(temp.path().join("test.py"), "def python_func():").unwrap();

        let tool = CodeSearchTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({
                "pattern": "func",
                "file_type": "rust"
            }))
            .await
            .unwrap();
        assert!(result.success);

        let output = parse_output(result.output);
        assert!(output.matches_returned > 0);
        assert!(output.matches.iter().all(|m| m.path.ends_with(".rs")));
    }

    #[tokio::test]
    async fn test_search_no_matches() {
        if !has_ripgrep() {
            return;
        }
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("test.txt"), "hello world").unwrap();

        let tool = CodeSearchTool::new(vec![temp.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "nonexistent_pattern_xyz" }))
            .await
            .unwrap();
        assert!(result.success);

        let output = parse_output(result.output);
        assert_eq!(output.matches_returned, 0);
        assert!(output.next_offset.is_none());
    }

    #[tokio::test]
    async fn test_search_pagination() {
        if !has_ripgrep() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let lines: String = (0..25).map(|i| format!("hit line {i}\n")).collect();
        fs::write(temp.path().join("big.txt"), lines).unwrap();

        let tool = CodeSearchTool::new(vec![temp.path().to_path_buf()]);

        let page1 = parse_output(
            tool.execute(serde_json::json!({
                "pattern": "hit",
                "max_results": 10
            }))
            .await
            .unwrap()
            .output,
        );
        assert_eq!(page1.matches_returned, 10);
        assert_eq!(page1.next_offset, Some(10));

        let page2 = parse_output(
            tool.execute(serde_json::json!({
                "pattern": "hit",
                "max_results": 10,
                "offset": 10
            }))
            .await
            .unwrap()
            .output,
        );
        assert_eq!(page2.matches_returned, 10);
        assert_eq!(page2.next_offset, Some(20));

        let page3 = parse_output(
            tool.execute(serde_json::json!({
                "pattern": "hit",
                "max_results": 10,
                "offset": 20
            }))
            .await
            .unwrap()
            .output,
        );
        assert_eq!(page3.matches_returned, 5);
        assert!(page3.next_offset.is_none());
    }

    #[tokio::test]
    async fn test_match_text_truncated() {
        if !has_ripgrep() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let long_line = "x".repeat(MAX_MATCH_CHARS * 4) + " marker";
        fs::write(temp.path().join("long.txt"), &long_line).unwrap();

        let tool = CodeSearchTool::new(vec![temp.path().to_path_buf()]);
        let output = parse_output(
            tool.execute(serde_json::json!({ "pattern": "marker" }))
                .await
                .unwrap()
                .output,
        );
        assert_eq!(output.matches_returned, 1);
        assert!(output.matches[0].match_text.chars().count() <= MAX_MATCH_CHARS + 1);
        assert!(output.matches[0].match_text.ends_with('…'));
    }

    #[tokio::test]
    async fn test_byte_budget_cuts_tail() {
        if !has_ripgrep() {
            return;
        }
        let temp = TempDir::new().unwrap();
        let line: String = "needle ".to_string() + &"y".repeat(MAX_MATCH_CHARS) + "\n";
        let body: String = line.repeat(500);
        fs::write(temp.path().join("flood.txt"), body).unwrap();

        let tool = CodeSearchTool::new(vec![temp.path().to_path_buf()]);
        let output = parse_output(
            tool.execute(serde_json::json!({
                "pattern": "needle",
                "max_results": 500
            }))
            .await
            .unwrap()
            .output,
        );
        assert!(output.truncated);
        assert!(output.truncation_reason.is_some());
        assert!(output.next_offset.is_some());
        let body_len = serde_json::to_vec(&output).unwrap().len();
        assert!(
            body_len <= MAX_TOTAL_BYTES,
            "body {body_len} > {MAX_TOTAL_BYTES}"
        );
    }

    #[tokio::test]
    async fn test_ignores_credential_globs() {
        if !has_ripgrep() {
            return;
        }
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("vault.enc"), "TOPSECRET=abc123").unwrap();
        fs::write(
            temp.path().join("gog-credentials.json"),
            "{\"key\":\"xyz\"}",
        )
        .unwrap();
        fs::write(temp.path().join(".env"), "TOPSECRET=fromenv").unwrap();
        fs::write(temp.path().join("public.txt"), "TOPSECRET visible").unwrap();

        let tool = CodeSearchTool::new(vec![temp.path().to_path_buf()]);
        let output = parse_output(
            tool.execute(serde_json::json!({ "pattern": "TOPSECRET" }))
                .await
                .unwrap()
                .output,
        );
        assert_eq!(output.matches_returned, 1);
        assert!(output.matches[0].path.ends_with("public.txt"));
    }

    #[test]
    fn test_truncate_chars_utf8_safe() {
        let (out, cut) = truncate_chars("café", 10);
        assert_eq!(out, "café");
        assert!(!cut);

        let (out, cut) = truncate_chars("café latte", 4);
        assert_eq!(out, "café…");
        assert!(cut);
    }

    #[test]
    fn test_apply_byte_budget_drops_tail() {
        let mk = |n: usize| SearchMatch {
            path: format!("file{n}.rs"),
            line: n,
            match_text: "x".repeat(100),
            context_before: Vec::new(),
            context_after: Vec::new(),
        };
        let (kept, cut) = apply_byte_budget(vec![mk(1), mk(2), mk(3)], 500);
        assert!(cut);
        assert!(kept.len() < 3);
    }
}
