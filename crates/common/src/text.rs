/// Truncate a string to at most `max_bytes` bytes, rounding down to a valid
/// UTF-8 char boundary. This avoids panics when slicing strings that contain
/// multi-byte characters.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if max_bytes >= s.len() {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Trim and truncate to at most `max_bytes` bytes (UTF-8 boundary aware),
/// appending a single ellipsis character when the input was truncated.
/// Returns an empty string when the trimmed input is empty.
pub fn truncate_with_ellipsis(s: &str, max_bytes: usize) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= max_bytes {
        trimmed.to_string()
    } else {
        format!("{}…", truncate_str(trimmed, max_bytes))
    }
}

/// Plain text from HTML: `<script>`/`<style>` bodies are dropped, block tags
/// (`br`, `p`, `div`, `li`, `tr`, headings) become line breaks and other tags
/// spaces, common entities are decoded, and whitespace runs collapse to one
/// newline or space.
pub fn html_to_text(html: &str) -> String {
    collapse_whitespace(&decode_entities(&strip_tags(html)))
}

const BLOCK_TAGS: &[&str] = &[
    "br", "p", "div", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6",
];

const RAW_TEXT_TAGS: &[&str] = &["script", "style"];

const ENTITIES: &[(&str, &str)] = &[
    ("&amp;", "&"),
    ("&lt;", "<"),
    ("&gt;", ">"),
    ("&quot;", "\""),
    ("&#39;", "'"),
    ("&apos;", "'"),
    ("&nbsp;", " "),
];

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut resume = 0;
    for (i, c) in html.char_indices() {
        if i < resume {
            continue;
        }
        let rest = &html[i..];
        if c != '<' || !opens_tag(rest) {
            out.push(c);
            continue;
        }
        let tag_end = rest.find('>').map_or(rest.len(), |gt| gt + 1);
        let closing = rest[1..].starts_with('/');
        let name = tag_name(&rest[1..tag_end]);
        let skip = match RAW_TEXT_TAGS.iter().find(|t| t.eq_ignore_ascii_case(name)) {
            Some(raw) if !closing => raw_text_end(rest, tag_end, raw),
            _ => tag_end,
        };
        let is_block = BLOCK_TAGS.iter().any(|t| t.eq_ignore_ascii_case(name));
        out.push(if is_block { '\n' } else { ' ' });
        resume = i + skip;
    }
    out
}

fn opens_tag(rest: &str) -> bool {
    rest[1..]
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!')
}

fn tag_name(tag: &str) -> &str {
    let name = tag.trim_start_matches('/');
    let len = name
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(name.len());
    &name[..len]
}

/// Byte offset in `rest` just past the `</name>` closing a raw-text element
/// whose opening tag ends at `from`, or the end of `rest` when it is unclosed.
/// The match starts on an ASCII `<`, so it always lands on a char boundary.
fn raw_text_end(rest: &str, from: usize, name: &str) -> usize {
    let close = format!("</{name}");
    rest.as_bytes()[from..]
        .windows(close.len())
        .position(|w| w.eq_ignore_ascii_case(close.as_bytes()))
        .map_or(rest.len(), |j| {
            let after = from + j;
            rest[after..]
                .find('>')
                .map_or(rest.len(), |gt| after + gt + 1)
        })
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        match ENTITIES.iter().find(|(entity, _)| tail.starts_with(entity)) {
            Some((entity, decoded)) => {
                out.push_str(decoded);
                rest = &tail[entity.len()..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending: Option<char> = None;
    for c in text.chars() {
        if c.is_whitespace() {
            pending = Some(match (pending, c) {
                (Some('\n'), _) | (_, '\n') => '\n',
                _ => ' ',
            });
            continue;
        }
        if let Some(gap) = pending.take().filter(|_| !out.is_empty()) {
            out.push(gap);
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_str_short_input_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_respects_char_boundary() {
        // "héllo" is 6 bytes (é = 2 bytes); a max of 2 bytes must back off
        // to 1 to avoid splitting the multi-byte character.
        assert_eq!(truncate_str("héllo", 2), "h");
    }

    #[test]
    fn truncate_with_ellipsis_short_input_no_ellipsis() {
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
    }

    #[test]
    fn truncate_with_ellipsis_appends_when_truncated() {
        assert_eq!(truncate_with_ellipsis("hello world", 5), "hello…");
    }

    #[test]
    fn truncate_with_ellipsis_trims_whitespace() {
        assert_eq!(truncate_with_ellipsis("  hello  ", 10), "hello");
    }

    #[test]
    fn truncate_with_ellipsis_empty_input() {
        assert_eq!(truncate_with_ellipsis("   ", 10), "");
    }

    #[test]
    fn html_to_text_strips_tags() {
        assert_eq!(html_to_text("<p>Hello <b>World</b></p>"), "Hello World");
    }

    #[test]
    fn html_to_text_strips_script_and_style() {
        let text =
            html_to_text("<p>Before</p><script>alert('x')</script><STYLE>.x{}</STYLE><p>After</p>");
        assert_eq!(text, "Before\nAfter");
    }

    #[test]
    fn html_to_text_decodes_entities() {
        assert_eq!(
            html_to_text("Tom &amp; Jerry &lt;3 &quot;friends&quot;"),
            "Tom & Jerry <3 \"friends\""
        );
    }

    #[test]
    fn html_to_text_decodes_each_entity_once() {
        assert_eq!(
            html_to_text("&amp;lt;b&amp;gt; &unknown;"),
            "&lt;b&gt; &unknown;"
        );
    }

    #[test]
    fn html_to_text_separates_adjacent_block_tags() {
        assert_eq!(html_to_text("<p>Hello</p><p>World</p>"), "Hello\nWorld");
        assert_eq!(html_to_text("<td>a</td><td>b</td>"), "a b");
        assert_eq!(html_to_text("<pre>x</pre><pre>y</pre>"), "x y");
    }

    #[test]
    fn html_to_text_breaks_lines_at_br() {
        assert_eq!(
            html_to_text("one<br>two<BR/>three<br />four"),
            "one\ntwo\nthree\nfour"
        );
    }

    #[test]
    fn html_to_text_collapses_whitespace_runs() {
        assert_eq!(html_to_text("  a \t\n\n b  <div> c </div>  "), "a\nb\nc");
    }

    #[test]
    fn html_to_text_keeps_a_bare_less_than_sign() {
        assert_eq!(html_to_text("a < b and c > d"), "a < b and c > d");
    }

    #[test]
    fn html_to_text_handles_case_changing_multibyte_input() {
        assert_eq!(
            html_to_text("İ<b>bold</b> café<SCRIPT>İ()</SCRIPT>İ"),
            "İ bold café İ"
        );
    }

    #[test]
    fn html_to_text_drops_an_unclosed_script_to_the_end() {
        assert_eq!(html_to_text("keep<script>never()"), "keep");
    }
}
