use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use tracing::{info, warn};
use workforce_common::secret::{current_key, scrub, ApiKeySource};
use workforce_common::text::html_to_text;

use super::{Tool, ToolResult};
use crate::security::external_content::wrap_external_content;
use crate::security::ssrf::{check_url_for_ssrf, ssrf_safe_client};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BODY_SIZE: usize = 5 * 1024 * 1024; // 5 MB
const DEFAULT_MAX_CHARS: usize = 50_000;

fn build_http_client() -> reqwest::Client {
    ssrf_safe_client(DEFAULT_TIMEOUT, true)
}

// ── web.search ──────────────────────────────────────────────────────

/// Where a person may add the Brave Search key the hub reads for them.
pub const BRAVE_KEY_VAULT_PATH: &str = "tools/brave_api_key";

/// Tool for searching the web via the Brave Search API. The key is asked for
/// on every search, so each caller searches with their own.
pub struct WebSearchTool {
    client: Arc<reqwest::Client>,
    api_key: ApiKeySource,
}

impl WebSearchTool {
    pub fn new(api_key: ApiKeySource) -> Self {
        Self {
            client: Arc::new(build_http_client()),
            api_key,
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web.search"
    }

    fn description(&self) -> &str {
        "Search the web using the Brave Search API. Returns titles, URLs, and snippets."
    }

    fn soft_deadline(&self) -> Option<std::time::Duration> {
        // A search API call should return in seconds; cap it so a hung request
        // can't freeze a research turn (the runner returns a retry hint).
        Some(std::time::Duration::from_secs(30))
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                },
                "count": {
                    "type": "integer",
                    "description": "Number of results (1-10, default 5)"
                },
                "country": {
                    "type": "string",
                    "description": "Country code for regional results (e.g. 'US', 'GB')"
                },
                "freshness": {
                    "type": "string",
                    "enum": ["pd", "pw", "pm", "py"],
                    "description": "Freshness filter"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();

        let query = match input["query"].as_str() {
            Some(q) if !q.is_empty() => q,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing or empty 'query' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        let Some(api_key) = current_key(&self.api_key) else {
            return Ok(ToolResult::failure(
                format!(
                    "web.search is not available: no Brave Search API key is set for this \
                     user. A person adds their own in My Settings (vault path \
                     {BRAVE_KEY_VAULT_PATH}), or an admin sets one for the agent or the \
                     install. Do not ask for the key in chat or try to store it yourself."
                ),
                start.elapsed().as_millis() as u64,
            ));
        };

        let count = input["count"]
            .as_u64()
            .unwrap_or(5)
            .clamp(1, 10)
            .to_string();

        let mut params: Vec<(&str, &str)> = vec![("q", query), ("count", &count)];

        let country = input["country"].as_str().map(String::from);
        if let Some(ref c) = country {
            params.push(("country", c));
        }

        let freshness = input["freshness"].as_str().map(String::from);
        if let Some(ref f) = freshness {
            params.push(("freshness", f));
        }

        let response = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", &api_key)
            .header("Accept", "application/json")
            .query(&params)
            .send()
            .await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match response {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Ok(ToolResult::failure(
                        format!(
                            "Brave Search API error ({}): {}",
                            status,
                            scrub(&body, Some(&api_key))
                        ),
                        duration_ms,
                    ));
                }

                let body: serde_json::Value = match resp.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        return Ok(ToolResult::failure(
                            format!("Failed to parse search response: {e}"),
                            duration_ms,
                        ))
                    }
                };

                let results: Vec<serde_json::Value> = body["web"]["results"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| {
                        serde_json::json!({
                            "title": r["title"],
                            "url": r["url"],
                            "snippet": r["description"],
                        })
                    })
                    .collect();

                info!(query = %query, count = results.len(), "Web search completed");

                // Wrap search results with prompt injection detection
                let results_text = serde_json::to_string_pretty(&results).unwrap_or_default();
                let scan = wrap_external_content(&results_text, "web.search");
                if scan.suspicious {
                    warn!(
                        query = %query,
                        score = scan.score,
                        hostile = scan.hostile,
                        matches = ?scan.matches.iter().map(|m| m.description).collect::<Vec<_>>(),
                        "Suspicious patterns detected in search results"
                    );
                }

                Ok(ToolResult::success(
                    serde_json::json!({
                        "results": results,
                        "_security": {
                            "source": "external",
                            "suspicious": scan.suspicious,
                            "hostile": scan.hostile,
                            "score": scan.score,
                        }
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                if e.is_timeout() {
                    Ok(ToolResult::failure("Search request timed out", duration_ms))
                } else {
                    Ok(ToolResult::failure(
                        format!("Search request failed: {e}"),
                        duration_ms,
                    ))
                }
            }
        }
    }
}

// ── web.fetch ───────────────────────────────────────────────────────

/// Tool for fetching a URL and extracting readable content.
pub struct WebFetchTool {
    client: Arc<reqwest::Client>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            client: Arc::new(build_http_client()),
        }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web.fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and extract readable content as markdown or plain text."
    }

    fn soft_deadline(&self) -> Option<std::time::Duration> {
        // Most pages return well under this; the cap catches a hung connection
        // or a never-finishing slow-drip response so it can't freeze the turn.
        Some(std::time::Duration::from_secs(45))
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL to fetch (must be http or https)"
                },
                "extract_mode": {
                    "type": "string",
                    "enum": ["markdown", "text"],
                    "description": "Output format: 'markdown' (default) or 'text'"
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Maximum characters to return (default 50000)"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();

        let url_str = match input["url"].as_str() {
            Some(u) if !u.is_empty() => u,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing or empty 'url' parameter",
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        // Validate URL
        let parsed_url = match url::Url::parse(url_str) {
            Ok(u) => u,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid URL: {e}"),
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        match parsed_url.scheme() {
            "http" | "https" => {}
            scheme => {
                return Ok(ToolResult::failure(
                    format!(
                        "Unsupported URL scheme '{scheme}'. Only http and https are supported."
                    ),
                    start.elapsed().as_millis() as u64,
                ))
            }
        }

        // SSRF protection: block requests to private/internal networks
        if let Err(reason) = check_url_for_ssrf(&parsed_url) {
            return Ok(ToolResult::failure(
                reason,
                start.elapsed().as_millis() as u64,
            ));
        }

        let extract_mode = input["extract_mode"].as_str().unwrap_or("markdown");
        let max_chars = input["max_chars"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_CHARS);

        let response = self.client.get(url_str).send().await;
        let duration_ms = start.elapsed().as_millis() as u64;

        match response {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    return Ok(ToolResult::failure(
                        format!("HTTP {} fetching {}", status, url_str),
                        duration_ms,
                    ));
                }

                let final_url = resp.url().to_string();
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("text/plain")
                    .to_string();

                // Check content-length if available
                if let Some(len) = resp.content_length() {
                    if len as usize > MAX_BODY_SIZE {
                        return Ok(ToolResult::failure(
                            format!("Response too large ({} bytes, max {})", len, MAX_BODY_SIZE),
                            duration_ms,
                        ));
                    }
                }

                // Read body with size limit
                let body_bytes = match read_body_limited(resp, MAX_BODY_SIZE).await {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        return Ok(ToolResult::failure(
                            format!("Failed to read response body: {e}"),
                            duration_ms,
                        ))
                    }
                };

                let ct_lower = content_type.to_lowercase();
                let is_binary = !ct_lower.starts_with("text/")
                    && !ct_lower.contains("json")
                    && !ct_lower.contains("xml")
                    && !ct_lower.contains("javascript")
                    && !ct_lower.contains("css");

                if is_binary {
                    return Ok(ToolResult::failure(
                        format!(
                            "Binary content not supported (content-type: {})",
                            content_type
                        ),
                        duration_ms,
                    ));
                }

                let body_text = String::from_utf8_lossy(&body_bytes).to_string();

                // Convert based on content type and extract mode
                let (content, title) = if ct_lower.contains("text/html") {
                    let title = extract_html_title(&body_text);
                    let converted = if extract_mode == "text" {
                        html_to_text(&body_text)
                    } else {
                        html_to_markdown(&body_text)
                    };
                    (converted, title)
                } else if ct_lower.contains("json") {
                    // Pretty-print JSON
                    let pretty = serde_json::from_str::<serde_json::Value>(&body_text)
                        .ok()
                        .and_then(|v| serde_json::to_string_pretty(&v).ok())
                        .unwrap_or(body_text);
                    (pretty, None)
                } else {
                    // Plain text passthrough
                    (body_text, None)
                };

                // Truncate if needed
                let truncated = content.len() > max_chars;
                let content = if truncated {
                    truncate_at_boundary(&content, max_chars)
                } else {
                    content
                };

                // Wrap with prompt injection detection
                let scan = wrap_external_content(&content, "web.fetch");
                if scan.suspicious {
                    warn!(
                        url = %final_url,
                        score = scan.score,
                        hostile = scan.hostile,
                        matches = ?scan.matches.iter().map(|m| m.description).collect::<Vec<_>>(),
                        "Suspicious patterns detected in fetched content"
                    );
                }

                info!(url = %final_url, chars = content.len(), truncated, suspicious = scan.suspicious, "Web fetch completed");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "content": scan.wrapped_content,
                        "url": final_url,
                        "title": title,
                        "content_type": content_type,
                        "truncated": truncated,
                        "_security": {
                            "source": "external",
                            "suspicious": scan.suspicious,
                            "hostile": scan.hostile,
                            "score": scan.score,
                        }
                    }),
                    duration_ms,
                ))
            }
            Err(e) => {
                if e.is_timeout() {
                    Ok(ToolResult::failure(
                        format!("Request timed out fetching {}", url_str),
                        duration_ms,
                    ))
                } else {
                    Ok(ToolResult::failure(
                        format!("Failed to fetch {}: {}", url_str, e),
                        duration_ms,
                    ))
                }
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Read response body with a size limit.
/// Read a response body, aborting mid-stream once it exceeds `max_size` — the
/// cap has to be enforced as it arrives, not after buffering. Shared with the
/// `http.*` tools, which face the same hostile-endpoint OOM.
pub(crate) async fn read_body_limited(resp: reqwest::Response, max_size: usize) -> Result<Vec<u8>> {
    use futures::StreamExt;

    let mut stream = resp.bytes_stream();
    let mut body = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| eyre::eyre!("Stream error: {e}"))?;
        if body.len() + chunk.len() > max_size {
            return Err(eyre::eyre!("Response body exceeds {} byte limit", max_size));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

/// Convert HTML to markdown using htmd.
fn html_to_markdown(html: &str) -> String {
    htmd::convert(html).unwrap_or_else(|_| html_to_text(html))
}

/// Extract the <title> content from HTML.
fn extract_html_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title")?.checked_add(6)?;
    let after_tag = lower[start..].find('>')?.checked_add(1)?;
    let content_start = start + after_tag;
    let end = lower[content_start..].find("</title>")?;
    let title = html[content_start..content_start + end].trim().to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// Find the largest byte index <= max that is a valid UTF-8 char boundary.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Truncate at a word or line boundary near max_chars.
fn truncate_at_boundary(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }

    // Find a safe char boundary first
    let safe_end = floor_char_boundary(s, max_chars);
    let truncated = &s[..safe_end];

    // Then find the last newline or space for clean truncation
    if let Some(pos) = truncated.rfind('\n') {
        format!("{}\n\n[Content truncated]", &s[..pos])
    } else if let Some(pos) = truncated.rfind(' ') {
        format!("{}\n\n[Content truncated]", &s[..pos])
    } else {
        format!("{}\n\n[Content truncated]", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::secret::fixed_key;

    // ── Metadata tests ──────────────────────────────────────────────

    #[test]
    fn search_tool_metadata() {
        let tool = WebSearchTool::new(fixed_key("test_key"));
        assert_eq!(tool.name(), "web.search");
        assert!(!tool.description().is_empty());
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], serde_json::json!(["query"]));
    }

    #[test]
    fn fetch_tool_metadata() {
        let tool = WebFetchTool::new();
        assert_eq!(tool.name(), "web.fetch");
        assert!(!tool.description().is_empty());
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], serde_json::json!(["url"]));
    }

    // ── Validation tests ────────────────────────────────────────────

    #[tokio::test]
    async fn search_without_a_key_for_the_caller_fails_naming_where_to_add_one() {
        for source in [Arc::new(|| None) as ApiKeySource, fixed_key("  ")] {
            let result = WebSearchTool::new(source)
                .execute(serde_json::json!({"query": "rust"}))
                .await
                .unwrap();
            assert!(!result.success);
            let error = result.error.unwrap();
            assert!(error.contains("tools/brave_api_key"), "{error}");
            assert!(error.contains("Do not ask for the key in chat"), "{error}");
        }
    }

    #[tokio::test]
    async fn search_missing_query_fails() {
        let tool = WebSearchTool::new(fixed_key("test_key"));
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("query"));
    }

    #[tokio::test]
    async fn search_empty_query_fails() {
        let tool = WebSearchTool::new(fixed_key("test_key"));
        let result = tool
            .execute(serde_json::json!({"query": ""}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn fetch_missing_url_fails() {
        let tool = WebFetchTool::new();
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("url"));
    }

    #[tokio::test]
    async fn fetch_invalid_scheme_fails() {
        let tool = WebFetchTool::new();
        let result = tool
            .execute(serde_json::json!({"url": "ftp://example.com/file"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unsupported URL scheme"));
    }

    #[tokio::test]
    async fn fetch_invalid_url_fails() {
        let tool = WebFetchTool::new();
        let result = tool
            .execute(serde_json::json!({"url": "not a url"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid URL"));
    }

    // ── HTML conversion tests ───────────────────────────────────────

    #[test]
    fn html_to_markdown_basic() {
        let html = "<h1>Hello</h1><p>World</p>";
        let md = html_to_markdown(html);
        assert!(md.contains("Hello"));
        assert!(md.contains("World"));
    }

    #[test]
    fn extract_title_basic() {
        let html = "<html><head><title>My Page</title></head><body></body></html>";
        assert_eq!(extract_html_title(html), Some("My Page".to_string()));
    }

    #[test]
    fn extract_title_missing() {
        let html = "<html><head></head><body></body></html>";
        assert_eq!(extract_html_title(html), None);
    }

    // ── Truncation tests ────────────────────────────────────────────

    #[test]
    fn truncate_short_content_unchanged() {
        let content = "Hello world";
        assert_eq!(truncate_at_boundary(content, 100), "Hello world");
    }

    #[test]
    fn truncate_at_newline() {
        let content = "Line one\nLine two\nLine three";
        let result = truncate_at_boundary(content, 15);
        assert!(result.starts_with("Line one"));
        assert!(result.contains("[Content truncated]"));
        assert!(!result.contains("Line three"));
    }

    #[test]
    fn truncate_at_space() {
        let content = "one two three four five";
        let result = truncate_at_boundary(content, 12);
        assert!(result.starts_with("one two"));
        assert!(result.contains("[Content truncated]"));
    }
}
