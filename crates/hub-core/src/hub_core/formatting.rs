//! Platform-specific message formatting pipeline.
//!
//! Converts standard markdown from LLM output to per-channel formatting:
//! - Slack mrkdwn
//! - Telegram HTML
//! - Discord markdown (mostly passthrough with table conversion)
//! - Plain text (CLI/fallback)
//!
//! Also handles format-aware chunking that preserves code blocks and lists
//! when splitting messages at platform size limits.
//!

use serde::{Deserialize, Serialize};

/// Target platform for formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// Slack mrkdwn syntax.
    Slack,
    /// Telegram HTML formatting.
    Telegram,
    /// Discord markdown (mostly standard with quirks).
    Discord,
    /// Plain text (CLI, fallback).
    Plain,
}

impl Platform {
    /// Maximum message length for this platform.
    pub fn max_message_length(&self) -> usize {
        match self {
            Self::Slack => 4000,    // Slack API limit
            Self::Telegram => 4096, // Telegram Bot API limit
            Self::Discord => 2000,  // Discord message limit
            Self::Plain => usize::MAX,
        }
    }
}

/// Format markdown text for a specific platform.
pub fn format_for_platform(markdown: &str, platform: Platform) -> String {
    match platform {
        Platform::Slack => to_slack_mrkdwn(markdown),
        Platform::Telegram => to_telegram_html(markdown),
        Platform::Discord => to_discord(markdown),
        Platform::Plain => strip_markdown(markdown),
    }
}

/// Split a message into chunks that fit within the platform's size limit,
/// preserving formatting state (code blocks, lists).
pub fn chunk_message(text: &str, platform: Platform) -> Vec<String> {
    let max_len = platform.max_message_length();
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }

        // Find a good split point
        let split_at = find_split_point(remaining, max_len);
        let (chunk, rest) = remaining.split_at(split_at);
        chunks.push(chunk.trim_end().to_string());
        remaining = rest.trim_start();
    }

    chunks
}

/// Find a good point to split text, preserving formatting.
fn find_split_point(text: &str, max_len: usize) -> usize {
    let max_len = text.floor_char_boundary(max_len);
    let search_range = &text[..max_len];

    // Check if we're inside a code block
    let code_fence_count = search_range.matches("```").count();
    let in_code_block = !code_fence_count.is_multiple_of(2);

    if in_code_block {
        // Try to find the end of the code block
        if let Some(fence_end) = search_range.rfind("```") {
            let after_fence = fence_end + 3;
            // Find newline after closing fence
            if let Some(nl) = text[after_fence..].find('\n') {
                let split = after_fence + nl + 1;
                if split <= max_len {
                    return split;
                }
            }
        }
    }

    // Try to split at a blank line (paragraph break)
    if let Some(pos) = search_range.rfind("\n\n") {
        return pos + 2;
    }

    // Try to split at a newline
    if let Some(pos) = search_range.rfind('\n') {
        return pos + 1;
    }

    // Try to split at a space
    if let Some(pos) = search_range.rfind(' ') {
        return pos + 1;
    }

    // Last resort: hard split
    max_len
}

// ─── Slack mrkdwn ────────────────────────────────────────────────────────────

/// Convert markdown to Slack mrkdwn format.
fn to_slack_mrkdwn(markdown: &str) -> String {
    let mut result = String::with_capacity(markdown.len());
    let mut in_code_block = false;
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();

    for line in markdown.lines() {
        // Track code blocks (pass through unchanged)
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            result.push_str(line);
            result.push('\n');
            continue;
        }

        if in_code_block {
            result.push_str(line);
            result.push('\n');
            continue;
        }

        // Detect markdown tables
        if line.contains('|') && !line.trim().is_empty() {
            let cells: Vec<String> = line
                .split('|')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect();

            // Skip separator rows (---|---|---)
            if cells
                .iter()
                .all(|c| c.chars().all(|ch| ch == '-' || ch == ':'))
            {
                continue;
            }

            if !cells.is_empty() {
                in_table = true;
                table_rows.push(cells);
                continue;
            }
        } else if in_table {
            // End of table — flush
            result.push_str(&table_to_slack_list(&table_rows));
            table_rows.clear();
            in_table = false;
        }

        // Convert markdown formatting to Slack mrkdwn
        let converted = convert_line_to_slack(line);
        result.push_str(&converted);
        result.push('\n');
    }

    // Flush any remaining table
    if !table_rows.is_empty() {
        result.push_str(&table_to_slack_list(&table_rows));
    }

    result.trim_end().to_string()
}

/// Convert a single line from markdown to Slack mrkdwn.
fn convert_line_to_slack(line: &str) -> String {
    let mut s = line.to_string();

    // Headers → bold
    if let Some(stripped) = s.strip_prefix("### ") {
        return format!("*{}*", stripped);
    }
    if let Some(stripped) = s.strip_prefix("## ") {
        return format!("*{}*", stripped);
    }
    if let Some(stripped) = s.strip_prefix("# ") {
        return format!("*{}*", stripped);
    }

    // Bold: **text** → *text* (Slack uses single asterisk)
    s = convert_bold_to_slack(&s);

    // Italic: *text* or _text_ → _text_ (Slack uses underscore)
    // Note: must do after bold conversion to avoid conflicts
    s = convert_italic_to_slack(&s);

    // Links: [text](url) → <url|text>
    s = convert_links_to_slack(&s);

    // Blockquotes: > text → > text (same in Slack)
    // Already correct format

    s
}

/// Convert **bold** to *bold* for Slack.
fn convert_bold_to_slack(text: &str) -> String {
    let mut result = String::new();
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '*' && chars.peek() == Some(&'*') {
            chars.next(); // consume second *
            result.push('*');
        } else {
            result.push(ch);
        }
    }

    result
}

/// Convert standalone *italic* (not **bold**) to _italic_ for Slack.
fn convert_italic_to_slack(text: &str) -> String {
    // Simple approach: single * that aren't part of ** → _
    // Since we've already converted ** to *, we need to be careful.
    // For now, leave as-is — Slack also uses *text* for bold.
    text.to_string()
}

/// Convert [text](url) links to Slack format <url|text>.
fn convert_links_to_slack(text: &str) -> String {
    let mut result = String::new();
    let mut remaining = text;

    while let Some(bracket_start) = remaining.find('[') {
        result.push_str(&remaining[..bracket_start]);
        let after_bracket = &remaining[bracket_start + 1..];

        if let Some(bracket_end) = after_bracket.find(']') {
            let link_text = &after_bracket[..bracket_end];
            let after_close = &after_bracket[bracket_end + 1..];

            if after_close.starts_with('(') {
                if let Some(paren_end) = after_close.find(')') {
                    let url = &after_close[1..paren_end];
                    result.push_str(&format!("<{}|{}>", url, link_text));
                    remaining = &after_close[paren_end + 1..];
                    continue;
                }
            }
            // Not a valid link
            result.push('[');
            remaining = after_bracket;
        } else {
            result.push('[');
            remaining = after_bracket;
        }
    }

    result.push_str(remaining);
    result
}

/// Convert a markdown table to a Slack-friendly bullet list.
fn table_to_slack_list(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let headers = &rows[0];
    let mut result = String::new();

    for row in rows.iter().skip(1) {
        let fields: Vec<String> = headers
            .iter()
            .zip(row.iter())
            .map(|(h, v)| format!("*{}*: {}", h, v))
            .collect();
        result.push_str(&format!("• {}\n", fields.join(" | ")));
    }

    result
}

// ─── Telegram HTML ───────────────────────────────────────────────────────────

/// Convert markdown to Telegram HTML format.
fn to_telegram_html(markdown: &str) -> String {
    let mut result = String::with_capacity(markdown.len());
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();

    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            if !in_code_block {
                in_code_block = true;
                code_lang = line
                    .trim_start()
                    .strip_prefix("```")
                    .unwrap_or("")
                    .to_string();
                if code_lang.is_empty() {
                    result.push_str("<pre>");
                } else {
                    result.push_str(&format!(
                        "<pre><code class=\"language-{}\">",
                        html_escape(&code_lang)
                    ));
                }
            } else {
                in_code_block = false;
                if code_lang.is_empty() {
                    result.push_str("</pre>\n");
                } else {
                    result.push_str("</code></pre>\n");
                }
                code_lang.clear();
            }
            continue;
        }

        if in_code_block {
            result.push_str(&html_escape(line));
            result.push('\n');
            continue;
        }

        // Table handling
        if line.contains('|') && !line.trim().is_empty() {
            let cells: Vec<String> = line
                .split('|')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect();

            if cells
                .iter()
                .all(|c| c.chars().all(|ch| ch == '-' || ch == ':'))
            {
                continue;
            }

            if !cells.is_empty() {
                in_table = true;
                table_rows.push(cells);
                continue;
            }
        } else if in_table {
            result.push_str(&table_to_telegram(&table_rows));
            table_rows.clear();
            in_table = false;
        }

        let converted = convert_line_to_telegram(line);
        result.push_str(&converted);
        result.push('\n');
    }

    if !table_rows.is_empty() {
        result.push_str(&table_to_telegram(&table_rows));
    }

    result.trim_end().to_string()
}

/// Convert a single line from markdown to Telegram HTML.
fn convert_line_to_telegram(line: &str) -> String {
    let mut s = html_escape(line);

    // Headers → bold
    if let Some(stripped) = s.strip_prefix("### ") {
        return format!("<b>{}</b>", stripped);
    }
    if let Some(stripped) = s.strip_prefix("## ") {
        return format!("<b>{}</b>", stripped);
    }
    if let Some(stripped) = s.strip_prefix("# ") {
        return format!("<b>{}</b>", stripped);
    }

    // Inline code: `text` → <code>text</code>
    s = convert_inline_code_to_telegram(&s);

    // Bold: **text** → <b>text</b>
    s = regex_replace_pairs(&s, "**", "<b>", "</b>");

    // Italic: *text* → <i>text</i>
    s = regex_replace_pairs(&s, "*", "<i>", "</i>");

    // Links: [text](url) → <a href="url">text</a>
    // (after html_escape, brackets are preserved)
    s = convert_links_to_telegram(&s);

    s
}

/// Simple paired delimiter replacement.
fn regex_replace_pairs(text: &str, delim: &str, open: &str, close: &str) -> String {
    let mut result = String::new();
    let mut remaining = text;
    let mut open_next = true;

    while let Some(pos) = remaining.find(delim) {
        result.push_str(&remaining[..pos]);
        if open_next {
            result.push_str(open);
        } else {
            result.push_str(close);
        }
        open_next = !open_next;
        remaining = &remaining[pos + delim.len()..];
    }

    result.push_str(remaining);
    result
}

/// Convert `inline code` to <code>inline code</code>.
fn convert_inline_code_to_telegram(text: &str) -> String {
    regex_replace_pairs(text, "`", "<code>", "</code>")
}

/// Convert [text](url) to Telegram HTML links.
fn convert_links_to_telegram(text: &str) -> String {
    let mut result = String::new();
    let mut remaining = text;

    while let Some(bracket_start) = remaining.find('[') {
        result.push_str(&remaining[..bracket_start]);
        let after_bracket = &remaining[bracket_start + 1..];

        if let Some(bracket_end) = after_bracket.find(']') {
            let link_text = &after_bracket[..bracket_end];
            let after_close = &after_bracket[bracket_end + 1..];

            if after_close.starts_with('(') {
                if let Some(paren_end) = after_close.find(')') {
                    let url = &after_close[1..paren_end];
                    result.push_str(&format!("<a href=\"{}\">{}</a>", url, link_text));
                    remaining = &after_close[paren_end + 1..];
                    continue;
                }
            }
            result.push('[');
            remaining = after_bracket;
        } else {
            result.push('[');
            remaining = after_bracket;
        }
    }

    result.push_str(remaining);
    result
}

/// Convert a table to Telegram-friendly format.
fn table_to_telegram(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let headers = &rows[0];
    let mut result = String::new();

    for row in rows.iter().skip(1) {
        let fields: Vec<String> = headers
            .iter()
            .zip(row.iter())
            .map(|(h, v)| format!("<b>{}</b>: {}", html_escape(h), html_escape(v)))
            .collect();
        result.push_str(&format!("• {}\n", fields.join(" | ")));
    }

    result
}

/// Escape HTML special characters.
pub fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ─── Discord ─────────────────────────────────────────────────────────────────

/// Convert markdown to Discord-compatible format.
/// Discord supports standard markdown mostly, but tables need conversion.
fn to_discord(markdown: &str) -> String {
    let mut result = String::with_capacity(markdown.len());
    let mut in_code_block = false;
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();

    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            result.push_str(line);
            result.push('\n');
            continue;
        }

        if in_code_block {
            result.push_str(line);
            result.push('\n');
            continue;
        }

        // Table handling — convert to bullet lists
        if line.contains('|') && !line.trim().is_empty() {
            let cells: Vec<String> = line
                .split('|')
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect();

            if cells
                .iter()
                .all(|c| c.chars().all(|ch| ch == '-' || ch == ':'))
            {
                continue;
            }

            if !cells.is_empty() {
                in_table = true;
                table_rows.push(cells);
                continue;
            }
        } else if in_table {
            result.push_str(&table_to_discord_list(&table_rows));
            table_rows.clear();
            in_table = false;
        }

        // Suppress link embeds for multiple URLs
        let converted = suppress_discord_embeds(line);
        result.push_str(&converted);
        result.push('\n');
    }

    if !table_rows.is_empty() {
        result.push_str(&table_to_discord_list(&table_rows));
    }

    result.trim_end().to_string()
}

/// Convert a table to Discord bullet list.
fn table_to_discord_list(rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let headers = &rows[0];
    let mut result = String::new();

    for row in rows.iter().skip(1) {
        let fields: Vec<String> = headers
            .iter()
            .zip(row.iter())
            .map(|(h, v)| format!("**{}**: {}", h, v))
            .collect();
        result.push_str(&format!("• {}\n", fields.join(" | ")));
    }

    result
}

/// Wrap bare URLs in <> to suppress Discord embeds.
fn suppress_discord_embeds(line: &str) -> String {
    // Count URLs in line — if more than 1, wrap all in <>
    let url_count = line.matches("http://").count() + line.matches("https://").count();
    if url_count <= 1 {
        return line.to_string();
    }

    let mut result = String::new();
    let mut remaining = line;

    while let Some(pos) = remaining.find("http") {
        // Check it's actually a URL
        let url_start = &remaining[pos..];
        if url_start.starts_with("http://") || url_start.starts_with("https://") {
            // Already wrapped?
            if pos > 0 && remaining.as_bytes()[pos - 1] == b'<' {
                result.push_str(&remaining[..pos]);
                // Find end of URL
                let end = url_start
                    .find(|c: char| c.is_whitespace() || c == '>')
                    .unwrap_or(url_start.len());
                result.push_str(&url_start[..end]);
                remaining = &url_start[end..];
            } else {
                result.push_str(&remaining[..pos]);
                let end = url_start
                    .find(char::is_whitespace)
                    .unwrap_or(url_start.len());
                result.push('<');
                result.push_str(&url_start[..end]);
                result.push('>');
                remaining = &url_start[end..];
            }
        } else {
            result.push_str(&remaining[..pos + 4]);
            remaining = &remaining[pos + 4..];
        }
    }

    result.push_str(remaining);
    result
}

// ─── Plain text ──────────────────────────────────────────────────────────────

/// Strip markdown formatting for plain text output.
/// Normalize text for TTS output: strip markdown formatting, remove emoji,
/// and collapse excess whitespace into clean spoken-word text.
pub fn normalize_for_tts(text: &str) -> String {
    let plain = strip_markdown(text);
    let no_emoji = workforce_common::emoji::strip_emoji(&plain);
    // Strip :shortcode: style emoji (e.g. ":fire:", ":thumbsup:")
    let no_shortcodes = strip_shortcode_emoji(&no_emoji);
    // Collapse multiple blank lines and internal runs of spaces
    let mut result = String::with_capacity(no_shortcodes.len());
    let mut prev_blank = false;
    for line in no_shortcodes.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !prev_blank {
                result.push('\n');
                prev_blank = true;
            }
        } else {
            // Collapse internal double-spaces left by removed shortcodes
            let collapsed = collapse_spaces(trimmed);
            result.push_str(&collapsed);
            result.push('\n');
            prev_blank = false;
        }
    }
    result.trim().to_string()
}

/// Collapse runs of multiple spaces into a single space.
fn collapse_spaces(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut prev_space = false;
    for c in text.chars() {
        if c == ' ' {
            if !prev_space {
                result.push(' ');
            }
            prev_space = true;
        } else {
            result.push(c);
            prev_space = false;
        }
    }
    result
}

/// Remove `:name:` shortcode emoji from text.
fn strip_shortcode_emoji(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == ':' {
            // Look ahead for closing colon (shortcode emoji are typically 2-30 chars)
            if let Some(end) = text[i + 1..].find(':') {
                let name = &text[i + 1..i + 1 + end];
                if !name.is_empty()
                    && !name.contains(' ')
                    && name.len() <= 30
                    && name
                        .chars()
                        .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-')
                {
                    // Skip past the closing colon
                    for _ in 0..=end {
                        chars.next();
                    }
                    continue;
                }
            }
            result.push(c);
        } else {
            result.push(c);
        }
    }
    result
}

/// Strip markdown markers and emoji from a single stream delta token.
/// Unlike `normalize_for_tts`, this preserves all whitespace so that
/// inter-token spacing is maintained when deltas are concatenated.
pub fn normalize_delta_for_tts(text: &str) -> String {
    text.chars()
        .filter(|&c| c != '*' && c != '_' && !workforce_common::emoji::is_emoji(c))
        .collect()
}

fn strip_markdown(markdown: &str) -> String {
    let mut result = String::with_capacity(markdown.len());
    let mut in_code_block = false;

    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            continue; // Skip code fences
        }

        if in_code_block {
            result.push_str(line);
            result.push('\n');
            continue;
        }

        // Strip headers
        let stripped = line
            .strip_prefix("### ")
            .or_else(|| line.strip_prefix("## "))
            .or_else(|| line.strip_prefix("# "))
            .unwrap_or(line);

        // Strip bold/italic markers
        let cleaned = stripped
            .replace("**", "")
            .replace("__", "")
            .replace(['*', '_'], "");

        result.push_str(&cleaned);
        result.push('\n');
    }

    result.trim_end().to_string()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Slack formatting ─────────────────────────────────────────────

    #[test]
    fn slack_headers_to_bold() {
        assert_eq!(to_slack_mrkdwn("# Title"), "*Title*");
        assert_eq!(to_slack_mrkdwn("## Subtitle"), "*Subtitle*");
        assert_eq!(to_slack_mrkdwn("### Section"), "*Section*");
    }

    #[test]
    fn slack_bold_conversion() {
        assert_eq!(to_slack_mrkdwn("**bold text**"), "*bold text*");
    }

    #[test]
    fn slack_code_blocks_preserved() {
        let input = "```rust\nfn main() {}\n```";
        assert_eq!(to_slack_mrkdwn(input), input);
    }

    #[test]
    fn slack_links_converted() {
        assert_eq!(
            to_slack_mrkdwn("[click here](https://example.com)"),
            "<https://example.com|click here>"
        );
    }

    #[test]
    fn slack_table_to_list() {
        let input = "| Name | Age |\n|---|---|\n| Alice | 30 |\n| Bob | 25 |";
        let result = to_slack_mrkdwn(input);
        assert!(result.contains("*Name*: Alice"));
        assert!(result.contains("*Age*: 30"));
        assert!(result.contains("*Name*: Bob"));
    }

    // ── Telegram formatting ──────────────────────────────────────────

    #[test]
    fn telegram_headers_to_bold() {
        assert_eq!(to_telegram_html("# Title"), "<b>Title</b>");
    }

    #[test]
    fn telegram_bold_conversion() {
        assert_eq!(to_telegram_html("**bold**"), "<b>bold</b>");
    }

    #[test]
    fn telegram_inline_code() {
        assert_eq!(to_telegram_html("`code`"), "<code>code</code>");
    }

    #[test]
    fn telegram_code_blocks() {
        let result = to_telegram_html("```rust\nlet x = 1;\n```");
        assert!(result.contains("<pre>"));
        assert!(result.contains("let x = 1;"));
    }

    #[test]
    fn telegram_html_escaping() {
        assert_eq!(to_telegram_html("a < b & c > d"), "a &lt; b &amp; c &gt; d");
    }

    #[test]
    fn telegram_links() {
        let result = to_telegram_html("[click](https://example.com)");
        assert!(result.contains("<a href=\"https://example.com\">click</a>"));
    }

    // ── Discord formatting ───────────────────────────────────────────

    #[test]
    fn discord_table_to_list() {
        let input = "| Name | Age |\n|---|---|\n| Alice | 30 |";
        let result = to_discord(input);
        assert!(result.contains("**Name**: Alice"));
    }

    #[test]
    fn discord_code_blocks_preserved() {
        let input = "```js\nconsole.log('hi')\n```";
        assert_eq!(to_discord(input), input);
    }

    #[test]
    fn discord_embed_suppression() {
        let line = "Check https://a.com and https://b.com";
        let result = suppress_discord_embeds(line);
        assert!(result.contains("<https://a.com>"));
        assert!(result.contains("<https://b.com>"));
    }

    #[test]
    fn discord_single_url_not_suppressed() {
        let line = "Check https://example.com";
        assert_eq!(suppress_discord_embeds(line), line);
    }

    // ── Plain text ───────────────────────────────────────────────────

    #[test]
    fn plain_strips_headers() {
        assert_eq!(strip_markdown("# Title"), "Title");
        assert_eq!(strip_markdown("## Sub"), "Sub");
    }

    #[test]
    fn plain_strips_bold() {
        assert_eq!(strip_markdown("**bold**"), "bold");
    }

    // ── Chunking ─────────────────────────────────────────────────────

    #[test]
    fn chunk_short_message() {
        let chunks = chunk_message("Hello, world!", Platform::Slack);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Hello, world!");
    }

    #[test]
    fn chunk_at_paragraph_break() {
        let text = format!("{}\n\n{}", "a".repeat(3000), "b".repeat(2000));
        let chunks = chunk_message(&text, Platform::Slack);
        assert!(
            chunks.len() >= 2,
            "Expected >= 2 chunks, got {}",
            chunks.len()
        );
        assert!(chunks[0].len() <= 4000);
    }

    #[test]
    fn chunk_preserves_all_content() {
        let text = "word ".repeat(1000);
        let chunks = chunk_message(&text, Platform::Discord);
        let reassembled: String = chunks.join(" ");
        // All words should be present (whitespace may differ)
        assert_eq!(
            reassembled.split_whitespace().count(),
            text.split_whitespace().count()
        );
    }

    // ── Platform detection ───────────────────────────────────────────

    #[test]
    fn platform_max_lengths() {
        assert_eq!(Platform::Slack.max_message_length(), 4000);
        assert_eq!(Platform::Telegram.max_message_length(), 4096);
        assert_eq!(Platform::Discord.max_message_length(), 2000);
    }

    #[test]
    fn format_for_platform_dispatch() {
        let md = "**bold** text";
        assert_eq!(format_for_platform(md, Platform::Slack), "*bold* text");
        assert_eq!(
            format_for_platform(md, Platform::Telegram),
            "<b>bold</b> text"
        );
        assert!(format_for_platform(md, Platform::Plain).contains("bold"));
    }

    #[test]
    fn normalize_for_tts_strips_markdown() {
        assert_eq!(normalize_for_tts("**bold** text"), "bold text");
        assert_eq!(normalize_for_tts("*emphasis* here"), "emphasis here");
        assert_eq!(normalize_for_tts("# Header\n\nBody"), "Header\n\nBody");
        assert_eq!(normalize_for_tts("## Sub\nText"), "Sub\nText");
    }

    #[test]
    fn normalize_for_tts_strips_emoji() {
        assert_eq!(normalize_for_tts("Hello 🦁 world"), "Hello world");
        assert_eq!(normalize_for_tts("🎉 Great news!"), "Great news!");
    }

    #[test]
    fn normalize_for_tts_strips_shortcodes() {
        assert_eq!(normalize_for_tts("Hello :fire: world"), "Hello world");
        assert_eq!(normalize_for_tts(":thumbsup: Thanks"), "Thanks");
    }

    #[test]
    fn normalize_for_tts_preserves_punctuation() {
        assert_eq!(
            normalize_for_tts("Hello, world! How are you?"),
            "Hello, world! How are you?"
        );
    }

    #[test]
    fn normalize_for_tts_collapses_blank_lines() {
        // Multiple blank lines collapse to a single blank line (natural pause)
        assert_eq!(normalize_for_tts("A\n\n\n\nB"), "A\n\nB");
        // Single blank line is preserved
        assert_eq!(normalize_for_tts("A\n\nB"), "A\n\nB");
    }

    // ── Delta TTS normalization ─────────────────────────────────────

    #[test]
    fn delta_tts_preserves_whitespace() {
        assert_eq!(normalize_delta_for_tts(" world"), " world");
        assert_eq!(normalize_delta_for_tts("Hello "), "Hello ");
        assert_eq!(normalize_delta_for_tts("  "), "  ");
    }

    #[test]
    fn delta_tts_strips_markdown_markers() {
        assert_eq!(normalize_delta_for_tts("**"), "");
        assert_eq!(normalize_delta_for_tts(" **world"), " world");
        assert_eq!(normalize_delta_for_tts("*italic*"), "italic");
        assert_eq!(normalize_delta_for_tts("__bold__"), "bold");
    }

    #[test]
    fn delta_tts_strips_emoji() {
        assert_eq!(normalize_delta_for_tts("\u{2728}"), ""); // ✨
        assert_eq!(normalize_delta_for_tts("Hello \u{1F600}"), "Hello "); // 😀
    }

    #[test]
    fn delta_tts_passes_normal_text() {
        assert_eq!(normalize_delta_for_tts("Hello"), "Hello");
        assert_eq!(normalize_delta_for_tts("Hello, world!"), "Hello, world!");
    }

    #[test]
    fn delta_tts_empty_after_strip() {
        assert_eq!(normalize_delta_for_tts("**"), "");
        assert_eq!(normalize_delta_for_tts("*"), "");
        assert_eq!(normalize_delta_for_tts("\u{1F525}"), ""); // 🔥
    }
}
