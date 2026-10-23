//! Slack link/session-key helpers and inbound/outbound mention rewriting.
//!
//! Outbound markdown → Block Kit rendering lives in [`super::rich_text`]; this
//! module owns the surrounding token machinery: permalink construction,
//! session-key parsing, inbound `<@U…>`/`<#C…>` → human-name substitution,
//! link-token rewriting and entity (un)escaping, and outbound
//! `@name`/`#channel` → Slack-token rewriting.

/// Generate a clickable Slack permalink from a channel ID and message timestamp.
///
/// Slack's permalink format strips the dot from the timestamp and prefixes with `p`.
///
/// # Examples
/// ```
/// # use workforce_skills::adapters::slack::formatting::slack_thread_url;
/// assert_eq!(
///     slack_thread_url("D0FAKEDMAA1", "1771133439.982339"),
///     "https://app.slack.com/archives/D0FAKEDMAA1/p1771133439982339"
/// );
/// ```
pub fn slack_thread_url(channel: &str, ts: &str) -> String {
    let ts_clean = ts.replace('.', "");
    format!("https://app.slack.com/archives/{}/p{}", channel, ts_clean)
}

/// Parse a session key like `"slack:workspace:channel:thread_ts"` into components.
///
/// Returns `(workspace_id, channel_id, Option<thread_ts>)`.
/// Returns `None` if the key doesn't start with `"slack:"` or has too few parts.
pub fn parse_slack_session_key(key: &str) -> Option<(&str, &str, Option<&str>)> {
    let parts: Vec<&str> = key.splitn(4, ':').collect();
    match parts.as_slice() {
        ["slack", ws, ch, ts] => Some((ws, ch, Some(ts))),
        ["slack", ws, ch] => Some((ws, ch, None)),
        _ => None,
    }
}

// ── Slack-syntax substitution ──────────────────────────────────────────────

/// Walk every `<@U…>` or `<#C…>` token in `text` and push the bare ID into
/// the caller's accumulator sets. Inline labels (`<#C1|name>`) still get
/// collected because callers also want to populate sender / channel fields
/// from the same lookup map.
pub fn collect_ref_ids(
    text: &str,
    user_ids: &mut std::collections::HashSet<String>,
    channel_ids: &mut std::collections::HashSet<String>,
) {
    for caps in slack_token_regex().captures_iter(text) {
        let Some(inner) = caps.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let head = inner.split_once('|').map(|(h, _)| h).unwrap_or(inner);
        match head.chars().next() {
            Some('@') => {
                user_ids.insert(head[1..].to_string());
            }
            Some('#') => {
                channel_ids.insert(head[1..].to_string());
            }
            _ => {}
        }
    }
}

fn slack_token_regex() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static TOKEN: OnceLock<regex::Regex> = OnceLock::new();
    TOKEN.get_or_init(|| regex::Regex::new(r"<([^<>]+)>").expect("static regex"))
}

/// Rewrite Slack `<…>` tokens in `text` to their human-readable form so the
/// agent (and any human reading the agent's output) sees real names instead
/// of opaque IDs. A channel or user group keeps its ID alongside the name —
/// `#ops (C123)` — because the agent may be asked to act on it.
///
/// `user_names` and `channel_names` are pre-resolved lookup maps populated by
/// the caller (typically via parallel `users.info` / `conversations.info`
/// calls). Keeping the resolution outside this function makes it pure and
/// trivially testable. IDs absent from the maps fall through unchanged —
/// the LLM then sees the raw token rather than a misleading substitution.
pub fn substitute_slack_refs(
    text: &str,
    user_names: &std::collections::HashMap<String, String>,
    channel_names: &std::collections::HashMap<String, String>,
) -> String {
    slack_token_regex()
        .replace_all(text, |caps: &regex::Captures<'_>| {
            let whole = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
            let inner = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            render_slack_token(inner, user_names, channel_names)
                .unwrap_or_else(|| whole.to_string())
        })
        .into_owned()
}

fn render_slack_token(
    inner: &str,
    user_names: &std::collections::HashMap<String, String>,
    channel_names: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let (head, label) = match inner.split_once('|') {
        Some((h, l)) => (h, Some(l)),
        None => (inner, None),
    };

    match head.chars().next()? {
        '@' => {
            let user_id = &head[1..];
            let resolved = label
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .or_else(|| user_names.get(user_id).cloned())?;
            Some(format!("@{resolved}"))
        }
        '#' => {
            let channel_id = &head[1..];
            let resolved = label
                .filter(|l| !l.is_empty())
                .map(|l| format!("#{l}"))
                .or_else(|| channel_names.get(channel_id).cloned())?;
            Some(format!("{resolved} ({channel_id})"))
        }
        '!' => render_broadcast_token(&head[1..], label),
        _ => render_link_token(head, label),
    }
}

fn render_broadcast_token(body: &str, label: Option<&str>) -> Option<String> {
    if matches!(body, "here" | "channel" | "everyone") {
        return Some(format!("@{body}"));
    }
    if let Some(rest) = body.strip_prefix("subteam^") {
        if let Some(l) = label.filter(|l| !l.is_empty()) {
            return Some(format!("@{} ({rest})", l.trim_start_matches('@')));
        }
        return Some(format!("@subteam-{rest}"));
    }
    if body.starts_with("date^") {
        return label.map(|l| l.to_string()).or_else(|| Some(body.into()));
    }
    None
}

fn is_link_url(url: &str) -> bool {
    url.starts_with("http://")
        || url.starts_with("https://")
        || url.starts_with("mailto:")
        || url.starts_with("tel:")
}

fn render_link_token(url: &str, label: Option<&str>) -> Option<String> {
    if !is_link_url(url) {
        return None;
    }
    Some(match label.filter(|l| !l.is_empty()) {
        Some(l) => l.to_string(),
        None => url.to_string(),
    })
}

/// Rewrite `<url>` / `<url|label>` link tokens to `url` / `label (url)`,
/// leaving mention/channel/broadcast tokens untouched for
/// [`substitute_slack_refs`], whose own link handling collapses a token to
/// its label and so loses the URL the agent needs.
pub fn preserve_link_urls(text: &str) -> String {
    slack_token_regex()
        .replace_all(text, |caps: &regex::Captures<'_>| {
            let whole = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
            let inner = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            let (url, label) = match inner.split_once('|') {
                Some((url, label)) => (url, Some(label)),
                None => (inner, None),
            };
            if !is_link_url(url) {
                return whole.to_string();
            }
            match label.filter(|l| !l.is_empty()) {
                Some(label) if label_restates_url(label, url) => label.to_string(),
                Some(label) => format!("{label} ({url})"),
                None => url.to_string(),
            }
        })
        .into_owned()
}

/// Slack autolinks a typed address into `<mailto:a@b.co|a@b.co>` or
/// `<http://example.com|example.com>`: the label is what the author wrote
/// and the URL adds only a scheme.
fn label_restates_url(label: &str, url: &str) -> bool {
    let bare = ["mailto:", "tel:", "https://", "http://"]
        .iter()
        .find_map(|scheme| url.strip_prefix(scheme))
        .unwrap_or(url);
    label == url || label.trim_end_matches('/') == bare.trim_end_matches('/')
}

/// Escape the three characters Slack escapes in message text, so a literal
/// string can sit alongside wire-form text without reading as a token.
pub fn escape_entities(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// An escaped `<` that would open a Slack token once unescaped. Text the
/// agent reads can be echoed back to Slack, where the outbound renderer turns
/// wire tokens into live mentions and broadcasts — so a `<!channel>` someone
/// typed literally must not come out looking like the real thing.
fn token_opener_regex() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static OPENER: OnceLock<regex::Regex> = OnceLock::new();
    OPENER.get_or_init(|| {
        regex::Regex::new(
            r"&lt;([@#][A-Z0-9]|!(?:here|channel|everyone|subteam\^|date\^)|https?://|mailto:|tel:)?",
        )
        .expect("static regex")
    })
}

/// Reverse Slack's entity escaping, except for a `&lt;` that would open a
/// token. `&amp;` goes last: unescaping it first would turn `&amp;lt;` into
/// a `<`.
fn unescape_entities(text: &str) -> String {
    token_opener_regex()
        .replace_all(text, |caps: &regex::Captures<'_>| match caps.get(1) {
            Some(opener) => format!("&lt;{}", opener.as_str()),
            None => "<".to_string(),
        })
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Last step for inbound message text read through the Web API: resolve the
/// remaining wire tokens, then unescape entities. The order matters — an
/// escaped `&lt;@U1&gt;` the author typed literally must not be resolved as a
/// mention.
pub fn finalize_inbound_text(
    text: &str,
    user_names: &std::collections::HashMap<String, String>,
    channel_names: &std::collections::HashMap<String, String>,
) -> String {
    unescape_entities(&substitute_slack_refs(text, user_names, channel_names))
}

// ── Outbound @mention / #channel rewriting ─────────────────────────────────

/// Maximum tokens we'll greedily try to match against a display name.
/// Slack display names with more than four whitespace-separated words are
/// vanishingly rare; the cap bounds candidate-window work.
const MAX_NAME_TOKENS: usize = 4;

#[derive(Debug, Eq, PartialEq)]
pub enum Segment<'a> {
    /// Substitutable text — outbound rewriter applies `@`/`#` substitutions here.
    Plain(&'a str),
    /// Verbatim region — code blocks, inline code, or already-formed Slack tokens.
    Skip(&'a str),
}

/// Split `text` into substitutable / skip segments. Skip covers anything
/// the outbound rewriter must not touch: triple-backtick blocks, inline
/// backticks, and pre-existing `<…>` Slack tokens.
pub fn segment_text(text: &str) -> Vec<Segment<'_>> {
    let bytes = text.as_bytes();
    let mut out: Vec<Segment<'_>> = Vec::new();
    let mut plain_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let skip_end = if bytes[i..].starts_with(b"```") {
            let after_open = i + 3;
            let close_rel = text[after_open..].find("```");
            Some(close_rel.map_or(bytes.len(), |rel| after_open + rel + 3))
        } else if bytes[i] == b'`' {
            let close_rel = text[i + 1..].find('`');
            Some(close_rel.map_or(bytes.len(), |rel| i + 1 + rel + 1))
        } else if bytes[i] == b'<' {
            text[i + 1..].find('>').map(|rel| i + 1 + rel + 1)
        } else {
            None
        };
        match skip_end {
            Some(end) => {
                if i > plain_start {
                    out.push(Segment::Plain(&text[plain_start..i]));
                }
                out.push(Segment::Skip(&text[i..end]));
                i = end;
                plain_start = i;
            }
            None => {
                i += 1;
            }
        }
    }
    if i > plain_start {
        out.push(Segment::Plain(&text[plain_start..i]));
    }
    out
}

/// Cheap pre-check used by the surface layer to skip the entire warm +
/// lookup pipeline when there's nothing to substitute.
pub fn needs_outbound_rewrite(text: &str) -> bool {
    text.contains('@') || text.contains('#')
}

/// Rewrite human-readable mentions in agent-authored text into Slack
/// wire-format tokens that actually generate notifications. See
/// [`Segment`] / [`segment_text`] for the skip-region semantics.
///
/// `users` / `channels` are lookup maps keyed by lower-cased display name.
/// Ambiguous names (multiple IDs for the same name) must be omitted by
/// the caller — only unambiguous matches end up substituted.
pub fn rewrite_outbound_mentions(
    text: &str,
    users: &std::collections::HashMap<String, String>,
    channels: &std::collections::HashMap<String, String>,
) -> String {
    segment_text(text)
        .into_iter()
        .map(|seg| match seg {
            Segment::Skip(s) => s.to_string(),
            Segment::Plain(s) => rewrite_plain_segment(s, users, channels),
        })
        .collect()
}

/// Collect candidate display-name strings the rewriter would attempt to
/// look up. Surface code uses this to populate the lookup maps before
/// invoking the pure substitution.
pub fn collect_outbound_candidates(text: &str) -> OutboundCandidates {
    let mut acc = OutboundCandidates::default();
    for seg in segment_text(text) {
        if let Segment::Plain(s) = seg {
            scan_plain_for_candidates(s, &mut acc);
        }
    }
    acc
}

#[derive(Debug, Default)]
pub struct OutboundCandidates {
    pub user_names: std::collections::HashSet<String>,
    pub channel_names: std::collections::HashSet<String>,
}

fn rewrite_plain_segment(
    text: &str,
    users: &std::collections::HashMap<String, String>,
    channels: &std::collections::HashMap<String, String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev: Option<char> = None;
    let mut iter = text.char_indices();
    while let Some((i, c)) = iter.next() {
        if (c == '@' || c == '#') && is_left_boundary(prev) {
            if let Some(special) = match_broadcast_token(&text[i..]) {
                out.push_str(special.replacement);
                advance_by(&mut iter, special.consumed.saturating_sub(c.len_utf8()));
                prev = Some('>');
                continue;
            }
            let lookup = if c == '@' { users } else { channels };
            if let Some(hit) = match_named_token(&text[i + c.len_utf8()..], lookup) {
                if c == '@' {
                    out.push_str(&format!("<@{}>", hit.id));
                } else {
                    out.push_str(&format!("<#{}|{}>", hit.id, hit.name));
                }
                advance_by(&mut iter, hit.consumed);
                prev = Some('>');
                continue;
            }
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

fn advance_by(iter: &mut std::str::CharIndices<'_>, bytes: usize) {
    let mut remaining = bytes;
    while remaining > 0 {
        match iter.next() {
            Some((_, c)) => remaining = remaining.saturating_sub(c.len_utf8()),
            None => break,
        }
    }
}

fn is_left_boundary(prev: Option<char>) -> bool {
    match prev {
        None => true,
        Some(c) => !(c.is_alphanumeric() || c == '_' || c == '@' || c == '#'),
    }
}

struct BroadcastHit {
    replacement: &'static str,
    consumed: usize,
}

fn match_broadcast_token(s: &str) -> Option<BroadcastHit> {
    for (token, replacement) in &[
        ("@here", "<!here>"),
        ("@channel", "<!channel>"),
        ("@everyone", "<!everyone>"),
    ] {
        if let Some(rest) = s.strip_prefix(token) {
            if is_right_boundary(rest) {
                return Some(BroadcastHit {
                    replacement,
                    consumed: token.len(),
                });
            }
        }
    }
    None
}

fn is_right_boundary(rest: &str) -> bool {
    match rest.as_bytes().first() {
        None => true,
        Some(&b) => !(b.is_ascii_alphanumeric() || b == b'_'),
    }
}

struct NamedHit<'a> {
    id: &'a str,
    name: String,
    consumed: usize,
}

/// Byte offsets at the end of each candidate-name token in `after_sigil`,
/// scanning up to [`MAX_NAME_TOKENS`] whitespace-separated tokens. Used by
/// both the substituter (which tries longest match first) and the candidate
/// collector (which records every prefix length).
fn name_token_ends(after_sigil: &str) -> Vec<usize> {
    let bytes = after_sigil.as_bytes();
    let mut ends = Vec::new();
    let mut idx = 0usize;
    while ends.len() < MAX_NAME_TOKENS && idx < bytes.len() {
        if !is_name_byte(bytes[idx]) {
            break;
        }
        while idx < bytes.len() && is_name_byte(bytes[idx]) {
            idx += 1;
        }
        ends.push(idx);
        let ws_start = idx;
        while idx < bytes.len() && (bytes[idx] == b' ' || bytes[idx] == b'\t') {
            idx += 1;
        }
        if idx == ws_start {
            break;
        }
    }
    ends
}

fn match_named_token<'a>(
    after_sigil: &str,
    lookup: &'a std::collections::HashMap<String, String>,
) -> Option<NamedHit<'a>> {
    let ends = name_token_ends(after_sigil);
    ends.iter().rev().find_map(|&end| {
        let candidate = after_sigil[..end].trim_end_matches('.');
        let key = candidate.to_lowercase();
        lookup.get(&key).map(|id| NamedHit {
            id,
            name: candidate.to_string(),
            consumed: end,
        })
    })
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'\'')
}

fn scan_plain_for_candidates(text: &str, acc: &mut OutboundCandidates) {
    let mut prev: Option<char> = None;
    for (i, c) in text.char_indices() {
        if (c == '@' || c == '#')
            && is_left_boundary(prev)
            && match_broadcast_token(&text[i..]).is_none()
        {
            let after = &text[i + c.len_utf8()..];
            if let Some(window) = candidate_window(after) {
                let bucket = if c == '@' {
                    &mut acc.user_names
                } else {
                    &mut acc.channel_names
                };
                for prefix in window {
                    bucket.insert(prefix.trim_end_matches('.').to_lowercase());
                }
            }
        }
        prev = Some(c);
    }
}

fn candidate_window(after_sigil: &str) -> Option<Vec<&str>> {
    let ends = name_token_ends(after_sigil);
    (!ends.is_empty()).then(|| ends.into_iter().map(|e| &after_sigil[..e]).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_thread_url_basic() {
        assert_eq!(
            slack_thread_url("D0FAKEDMAA1", "1771133439.982339"),
            "https://app.slack.com/archives/D0FAKEDMAA1/p1771133439982339"
        );
    }

    #[test]
    fn slack_thread_url_no_dot() {
        assert_eq!(
            slack_thread_url("C123", "1234567890123456"),
            "https://app.slack.com/archives/C123/p1234567890123456"
        );
    }

    #[test]
    fn parse_slack_session_key_with_thread() {
        let (ws, ch, ts) = parse_slack_session_key("slack:T123:C456:100.001").unwrap();
        assert_eq!(ws, "T123");
        assert_eq!(ch, "C456");
        assert_eq!(ts, Some("100.001"));
    }

    #[test]
    fn parse_slack_session_key_without_thread() {
        let (ws, ch, ts) = parse_slack_session_key("slack:T123:C456").unwrap();
        assert_eq!(ws, "T123");
        assert_eq!(ch, "C456");
        assert_eq!(ts, None);
    }

    #[test]
    fn parse_slack_session_key_invalid() {
        assert!(parse_slack_session_key("cli:test").is_none());
        assert!(parse_slack_session_key("notslack").is_none());
    }

    // ── preserve_link_urls ─────────────────────────────────────────────

    #[test]
    fn preserve_link_urls_rewrites_links_and_keeps_other_tokens() {
        assert_eq!(
            preserve_link_urls(
                "see <https://x.example/logs|the logs> <@U1> <#C1|deploys> <!here> <https://y.example>"
            ),
            "see the logs (https://x.example/logs) <@U1> <#C1|deploys> <!here> https://y.example"
        );
    }

    #[test]
    fn autolinked_address_reads_as_typed() {
        assert_eq!(
            preserve_link_urls(
                "<mailto:a.b@example.org|a.b@example.org> <http://example.com|example.com> \
                 <https://example.com/|example.com> <tel:+441234|+441234> \
                 <https://example.com|Example>"
            ),
            "a.b@example.org example.com example.com +441234 Example (https://example.com)"
        );
    }

    #[test]
    fn finalize_unescapes_entities_after_token_pass() {
        let users = std::collections::HashMap::from([("U1".to_string(), "Sam".to_string())]);
        let none = std::collections::HashMap::new();
        assert_eq!(
            finalize_inbound_text(
                "<@U1> typed &lt;@U1&gt; &amp;amp; a &gt; b &lt; c",
                &users,
                &none
            ),
            "@Sam typed &lt;@U1> &amp; a > b < c"
        );
    }

    #[test]
    fn escape_entities_round_trips_through_finalize() {
        let none = std::collections::HashMap::new();
        let literal = "if a < b && c > d { Vec<T> } <!-- note --> <!DOCTYPE html>";
        assert_eq!(
            finalize_inbound_text(&escape_entities(literal), &none, &none),
            literal
        );
    }

    #[test]
    fn literally_typed_tokens_stay_inert() {
        let typed = "&lt;!channel&gt; &lt;@U123&gt; &lt;#C123&gt; &lt;https://x.example|click&gt; \
                     &lt;!subteam^S1&gt;";
        assert_eq!(
            unescape_entities(typed),
            "&lt;!channel> &lt;@U123> &lt;#C123> &lt;https://x.example|click> &lt;!subteam^S1>"
        );
        assert!(!slack_token_regex().is_match(&unescape_entities(typed)));
    }

    #[test]
    fn preserve_link_urls_collapses_label_matching_url() {
        assert_eq!(
            preserve_link_urls("<https://x.example|https://x.example>"),
            "https://x.example"
        );
    }

    // ── substitute_slack_refs ──────────────────────────────────────────

    fn users(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }
    fn channels(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn user_mention_resolved_to_name() {
        let u = users(&[("U1", "Kingsley Hendrickse")]);
        let out = substitute_slack_refs("hi <@U1>", &u, &channels(&[]));
        assert_eq!(out, "hi @Kingsley Hendrickse");
    }

    #[test]
    fn user_mention_with_inline_label_wins_over_map() {
        let u = users(&[("U1", "FromMap")]);
        let out = substitute_slack_refs("<@U1|InlineLabel>", &u, &channels(&[]));
        assert_eq!(out, "@InlineLabel");
    }

    #[test]
    fn unresolvable_user_left_as_is() {
        let out = substitute_slack_refs("ping <@U_GHOST>", &users(&[]), &channels(&[]));
        assert_eq!(out, "ping <@U_GHOST>");
    }

    #[test]
    fn channel_with_inline_label_uses_label() {
        let out = substitute_slack_refs(
            "see <#C1|engineering>",
            &users(&[]),
            &channels(&[("C1", "#wrong")]),
        );
        assert_eq!(out, "see #engineering (C1)");
    }

    #[test]
    fn channel_without_label_uses_map() {
        let c = channels(&[("C1", "#engineering")]);
        let out = substitute_slack_refs("see <#C1>", &users(&[]), &c);
        assert_eq!(out, "see #engineering (C1)");
    }

    #[test]
    fn unresolvable_channel_left_as_is() {
        let out = substitute_slack_refs("see <#C_GHOST>", &users(&[]), &channels(&[]));
        assert_eq!(out, "see <#C_GHOST>");
    }

    #[test]
    fn broadcast_tokens_unwrap() {
        let u = users(&[]);
        let c = channels(&[]);
        assert_eq!(substitute_slack_refs("<!here>", &u, &c), "@here");
        assert_eq!(substitute_slack_refs("<!channel>", &u, &c), "@channel");
        assert_eq!(substitute_slack_refs("<!everyone>", &u, &c), "@everyone");
    }

    #[test]
    fn subteam_uses_label_and_keeps_id() {
        let out = substitute_slack_refs("<!subteam^S99|@design>", &users(&[]), &channels(&[]));
        assert_eq!(out, "@design (S99)");
    }

    #[test]
    fn link_with_label_keeps_label_only() {
        let out = substitute_slack_refs(
            "see <https://example.com|the docs>",
            &users(&[]),
            &channels(&[]),
        );
        assert_eq!(out, "see the docs");
    }

    #[test]
    fn link_without_label_keeps_url() {
        let out = substitute_slack_refs("see <https://example.com>", &users(&[]), &channels(&[]));
        assert_eq!(out, "see https://example.com");
    }

    #[test]
    fn mailto_link_unwrap() {
        let out = substitute_slack_refs("<mailto:a@b.com|email me>", &users(&[]), &channels(&[]));
        assert_eq!(out, "email me");
    }

    #[test]
    fn unknown_token_left_as_is() {
        // Slack sometimes nests angle brackets in code blocks; the regex
        // matches them but the renderer leaves anything it doesn't recognise.
        let out = substitute_slack_refs("<not-a-slack-token>", &users(&[]), &channels(&[]));
        assert_eq!(out, "<not-a-slack-token>");
    }

    #[test]
    fn mixed_tokens_in_one_pass() {
        let u = users(&[("U1", "Allan"), ("U2", "Mat")]);
        let c = channels(&[("C9", "#dev")]);
        let out = substitute_slack_refs(
            "<@U1> told <@U2> to check <#C9> via <https://example.com|docs>",
            &u,
            &c,
        );
        assert_eq!(out, "@Allan told @Mat to check #dev (C9) via docs");
    }

    // ── segment_text ─────────────────────────────────────────────────────

    #[test]
    fn segment_plain_only() {
        assert_eq!(
            segment_text("hello world"),
            vec![Segment::Plain("hello world")]
        );
    }

    #[test]
    fn segment_triple_backtick_block() {
        let segs = segment_text("before\n```\n@here\n```\nafter");
        assert_eq!(
            segs,
            vec![
                Segment::Plain("before\n"),
                Segment::Skip("```\n@here\n```"),
                Segment::Plain("\nafter"),
            ]
        );
    }

    #[test]
    fn segment_inline_code() {
        let segs = segment_text("see `@Mat` for details");
        assert_eq!(
            segs,
            vec![
                Segment::Plain("see "),
                Segment::Skip("`@Mat`"),
                Segment::Plain(" for details"),
            ]
        );
    }

    #[test]
    fn segment_existing_slack_token() {
        let segs = segment_text("hi <@U123> please");
        assert_eq!(
            segs,
            vec![
                Segment::Plain("hi "),
                Segment::Skip("<@U123>"),
                Segment::Plain(" please"),
            ]
        );
    }

    #[test]
    fn segment_unclosed_code_runs_to_end() {
        let segs = segment_text("hello ```still going");
        assert_eq!(
            segs,
            vec![Segment::Plain("hello "), Segment::Skip("```still going")]
        );
    }

    // ── rewrite_outbound_mentions ────────────────────────────────────────

    fn outbound_users(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, id)| (name.to_lowercase(), (*id).into()))
            .collect()
    }
    fn outbound_channels(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, id)| (name.to_lowercase(), (*id).into()))
            .collect()
    }

    #[test]
    fn outbound_here_channel_everyone() {
        let u = outbound_users(&[]);
        let c = outbound_channels(&[]);
        assert_eq!(
            rewrite_outbound_mentions("@here ping", &u, &c),
            "<!here> ping"
        );
        assert_eq!(
            rewrite_outbound_mentions("attention @channel", &u, &c),
            "attention <!channel>"
        );
        assert_eq!(
            rewrite_outbound_mentions("@everyone now", &u, &c),
            "<!everyone> now"
        );
    }

    #[test]
    fn outbound_single_word_name() {
        let u = outbound_users(&[("Kingsley", "U1")]);
        let out = rewrite_outbound_mentions("hi @Kingsley", &u, &outbound_channels(&[]));
        assert_eq!(out, "hi <@U1>");
    }

    #[test]
    fn outbound_multi_word_longest_prefix_wins() {
        let u = outbound_users(&[("Kingsley", "U1"), ("Kingsley Hendrickse", "U2")]);
        let out = rewrite_outbound_mentions("hi @Kingsley Hendrickse", &u, &outbound_channels(&[]));
        assert_eq!(out, "hi <@U2>");
    }

    #[test]
    fn outbound_case_insensitive_match() {
        let u = outbound_users(&[("Kingsley Hendrickse", "U1")]);
        let out =
            rewrite_outbound_mentions("@KINGSLEY HENDRICKSE here", &u, &outbound_channels(&[]));
        assert_eq!(out, "<@U1> here");
    }

    #[test]
    fn outbound_unknown_name_left_raw() {
        let u = outbound_users(&[("Kingsley", "U1")]);
        let out = rewrite_outbound_mentions("@Stranger said hi", &u, &outbound_channels(&[]));
        assert_eq!(out, "@Stranger said hi");
    }

    #[test]
    fn outbound_email_not_a_mention() {
        let u = outbound_users(&[("example", "U1"), ("example.com", "U2")]);
        let out = rewrite_outbound_mentions(
            "mail support@example.com today",
            &u,
            &outbound_channels(&[]),
        );
        assert_eq!(out, "mail support@example.com today");
    }

    #[test]
    fn outbound_markdown_emphasis_around_mention() {
        let u = outbound_users(&[("Kingsley", "U1")]);
        let out = rewrite_outbound_mentions("*@Kingsley*", &u, &outbound_channels(&[]));
        assert_eq!(out, "*<@U1>*");
    }

    #[test]
    fn outbound_inline_code_left_alone() {
        let u = outbound_users(&[("Kingsley", "U1")]);
        let out = rewrite_outbound_mentions("`@Kingsley`", &u, &outbound_channels(&[]));
        assert_eq!(out, "`@Kingsley`");
    }

    #[test]
    fn outbound_code_block_left_alone() {
        let u = outbound_users(&[("Kingsley", "U1")]);
        let out = rewrite_outbound_mentions(
            "before\n```\n@Kingsley\n```\nafter @Kingsley",
            &u,
            &outbound_channels(&[]),
        );
        assert_eq!(out, "before\n```\n@Kingsley\n```\nafter <@U1>");
    }

    #[test]
    fn outbound_existing_token_never_double_wrapped() {
        let u = outbound_users(&[("Kingsley", "U1")]);
        let out = rewrite_outbound_mentions("<@U1> please", &u, &outbound_channels(&[]));
        assert_eq!(out, "<@U1> please");
    }

    #[test]
    fn outbound_channel_rewrite_uses_id_pipe_name() {
        let c = outbound_channels(&[("engineering", "C123")]);
        let out = rewrite_outbound_mentions("see #engineering", &outbound_users(&[]), &c);
        assert_eq!(out, "see <#C123|engineering>");
    }

    #[test]
    fn outbound_trailing_punctuation_preserved() {
        let u = outbound_users(&[("Kingsley", "U1")]);
        let out = rewrite_outbound_mentions("@Kingsley, can you?", &u, &outbound_channels(&[]));
        assert_eq!(out, "<@U1>, can you?");
    }

    #[test]
    fn outbound_mixed_user_channel_broadcast() {
        let u = outbound_users(&[("Mat Jaggard", "U2")]);
        let c = outbound_channels(&[("engineering", "C9")]);
        let out = rewrite_outbound_mentions("@here ping @Mat Jaggard about #engineering", &u, &c);
        assert_eq!(out, "<!here> ping <@U2> about <#C9|engineering>");
    }

    #[test]
    fn outbound_empty_input_returns_empty() {
        assert_eq!(
            rewrite_outbound_mentions("", &outbound_users(&[]), &outbound_channels(&[])),
            ""
        );
    }

    #[test]
    fn needs_outbound_rewrite_short_circuits_correctly() {
        assert!(!needs_outbound_rewrite("nothing here"));
        assert!(needs_outbound_rewrite("call @somebody"));
        assert!(needs_outbound_rewrite("see #somewhere"));
    }

    #[test]
    fn collect_candidates_finds_window_prefixes() {
        let cands = collect_outbound_candidates("hi @Mat Jaggard and #eng-prod-deploys");
        assert!(cands.user_names.contains("mat"));
        assert!(cands.user_names.contains("mat jaggard"));
        assert!(cands.channel_names.contains("eng-prod-deploys"));
    }

    #[test]
    fn collect_candidates_skips_code_block() {
        let cands = collect_outbound_candidates("```\n@Mat\n```");
        assert!(cands.user_names.is_empty());
    }

    #[test]
    fn collect_candidates_skips_inline_code() {
        let cands = collect_outbound_candidates("see `@Mat` text");
        assert!(cands.user_names.is_empty());
    }
}
