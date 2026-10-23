//! Inbound message content rendering.
//!
//! Turns a raw Slack message — `text`, Block Kit `blocks`, legacy
//! `attachments` — into the text an agent reads. Output is in Slack wire
//! form: mention/channel/broadcast tokens and `&amp;`/`&lt;`/`&gt;` entities
//! are kept so the caller can resolve and unescape them in one place
//! ([`super::formatting::finalize_inbound_text`], or
//! [`InboundContent::finalized`] for an event read as it arrives); link
//! tokens are already rewritten to `label (url)`. Literal sources are entity-escaped on the way in so they
//! can never be read as tokens.
//!
//! A human message's `text` is Slack's own serialisation of what was typed,
//! and its `rich_text` block mirrors it; every other block and every
//! attachment that is not a link unfurl is content `text` does not carry. A
//! bot's `text` is only a notification fallback, so its blocks are the
//! content. Message shares quote another author and are kept apart from the
//! body so a caller can frame them as third-party content.

use serde_json::Value;

use std::collections::HashMap;

use super::formatting::{escape_entities, finalize_inbound_text, preserve_link_urls};

/// The readable content of one message: its own body, and any messages it
/// shares, each rendered as a framed quote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InboundContent {
    pub body: String,
    pub shared: Vec<String>,
}

impl InboundContent {
    pub fn is_empty(&self) -> bool {
        self.body.trim().is_empty() && self.shared.is_empty()
    }

    pub fn into_text(self) -> String {
        join_nonempty(std::iter::once(self.body).chain(self.shared))
    }

    fn map(self, f: impl Fn(&str) -> String) -> Self {
        Self {
            body: f(&self.body),
            shared: self.shared.iter().map(|s| f(s)).collect(),
        }
    }

    pub fn map_body(self, f: impl FnOnce(&str) -> String) -> Self {
        Self {
            body: f(&self.body),
            ..self
        }
    }

    /// Finalise with no name lookups — for a reader that receives an event
    /// as it arrives. Tokens that carry their own label resolve; a bare
    /// `<@U…>` or `<#C…>` stays in wire form, ID intact.
    pub fn finalized(self) -> Self {
        let none = HashMap::new();
        self.map(|text| finalize_inbound_text(text, &none, &none))
    }
}

/// Compose the readable text of a raw Slack message object as returned by
/// `conversations.history` / `conversations.replies` / `pins.list` /
/// `search.messages`.
pub fn compose_message_text(message: &Value) -> String {
    let is_bot = message["bot_id"].as_str().is_some_and(|b| !b.is_empty())
        || message["subtype"].as_str() == Some("bot_message");
    compose(
        message["text"].as_str().unwrap_or(""),
        is_bot,
        &message["blocks"],
        &message["attachments"],
    )
    .into_text()
}

/// Compose readable content from the parts of an already-parsed message
/// event. `blocks` / `attachments` are the raw JSON values (anything
/// non-array, including `Null`, is treated as absent).
pub fn compose(text: &str, is_bot: bool, blocks: &Value, attachments: &Value) -> InboundContent {
    let blocks = as_slice(blocks);
    let (shares, others): (Vec<&Value>, Vec<&Value>) =
        as_slice(attachments).iter().partition(|a| is_share(a));
    let primary = match is_bot {
        true => composed_body(text, blocks.iter()),
        false => typed_body(text, blocks),
    };
    let extras = others
        .into_iter()
        .filter(|a| is_bot || !is_unfurl(a))
        .filter_map(render_attachment);
    InboundContent {
        body: join_nonempty(std::iter::once(primary).chain(extras)),
        shared: shares.into_iter().map(render_share).collect(),
    }
    .map(preserve_link_urls)
}

fn typed_body(text: &str, blocks: &[Value]) -> String {
    let mirrored = !text.trim().is_empty();
    let rendered = render_blocks(
        blocks
            .iter()
            .filter(|b| !(mirrored && b["type"].as_str() == Some("rich_text"))),
    );
    join_nonempty([text.to_string(), rendered.text.unwrap_or_default()].into_iter())
}

fn composed_body<'a>(text: &str, blocks: impl Iterator<Item = &'a Value>) -> String {
    let rendered = render_blocks(blocks);
    match rendered.text {
        Some(body) if rendered.incomplete => join_nonempty([body, text.to_string()].into_iter()),
        Some(body) => body,
        None => text.to_string(),
    }
}

fn as_slice(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

fn nonempty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

fn push_nonempty(parts: &mut Vec<String>, s: &str) {
    if !s.trim().is_empty() {
        parts.push(s.to_string());
    }
}

fn join_parts(parts: Vec<String>) -> Option<String> {
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn join_nonempty(parts: impl Iterator<Item = String>) -> String {
    parts
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn quote(text: &str) -> String {
    text.lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A message-share attachment quotes another message — a forward or a pasted
/// permalink that Slack unfurled.
fn is_share(attachment: &Value) -> bool {
    attachment["is_share"].as_bool() == Some(true)
        || attachment["is_msg_unfurl"].as_bool() == Some(true)
}

/// A link unfurl is a preview Slack or an app generated for a URL in the
/// message — not something the author wrote or attached.
fn is_unfurl(attachment: &Value) -> bool {
    attachment["is_app_unfurl"].as_bool() == Some(true)
        || ["from_url", "original_url", "app_unfurl_url"]
            .iter()
            .any(|key| nonempty(attachment[*key].as_str()).is_some())
}

fn file_label(file: &Value) -> String {
    let name = nonempty(file["name"].as_str())
        .or_else(|| nonempty(file["title"].as_str()))
        .unwrap_or("file");
    format!("[file: {}]", escape_entities(name))
}

/// The shared message's own blocks. Slack sends `message_blocks` as a list
/// of `{message: {blocks}}` entries; a single such object is accepted too.
fn share_blocks(attachment: &Value) -> impl Iterator<Item = &Value> {
    let entries = &attachment["message_blocks"];
    entries
        .as_array()
        .map_or(std::slice::from_ref(entries), Vec::as_slice)
        .iter()
        .flat_map(|entry| as_slice(&entry["message"]["blocks"]))
}

/// Render a message share as a framed quote instead of the generic
/// attachment fields (whose footer would read "Slack Conversation"). The
/// share's `text` is the original's fallback, so its blocks are the content.
fn render_share(attachment: &Value) -> String {
    let body = Some(join_nonempty(
        std::iter::once(composed_body(
            attachment["text"].as_str().unwrap_or(""),
            share_blocks(attachment),
        ))
        .chain(as_slice(&attachment["files"]).iter().map(file_label)),
    ))
    .filter(|body| !body.is_empty())
    .unwrap_or_else(|| "[no readable content]".to_string());
    let origin = [
        (" from ", &attachment["author_name"]),
        (" in #", &attachment["channel_name"]),
    ]
    .iter()
    .filter_map(|(lead, name)| {
        Some(format!(
            "{lead}{}",
            escape_entities(nonempty(name.as_str())?)
        ))
    })
    .collect::<String>();
    format!("[shared message{origin}]\n{}", quote(&body))
}

/// What one block contributes: content, nothing by design (a divider, an
/// input), or a marker for a block type this renderer does not know.
enum Rendered {
    Content(String),
    Silent,
    Unrendered(String),
}

impl Rendered {
    fn from_parts(parts: Vec<String>) -> Self {
        join_parts(parts).map_or(Self::Silent, Self::Content)
    }

    fn from_option(text: Option<String>) -> Self {
        text.map_or(Self::Silent, Self::Content)
    }
}

/// The text of a run of blocks, and whether any of them could not be
/// rendered — the cue to keep the message's fallback text as well.
struct RenderedBlocks {
    text: Option<String>,
    incomplete: bool,
}

fn render_blocks<'a>(blocks: impl Iterator<Item = &'a Value>) -> RenderedBlocks {
    let rendered: Vec<Rendered> = blocks.map(render_block).collect();
    RenderedBlocks {
        incomplete: rendered
            .iter()
            .any(|r| matches!(r, Rendered::Unrendered(_))),
        text: join_parts(
            rendered
                .into_iter()
                .filter_map(|r| match r {
                    Rendered::Content(text) | Rendered::Unrendered(text) => Some(text),
                    Rendered::Silent => None,
                })
                .collect(),
        ),
    }
}

/// A Block Kit text object. `plain_text` is literal; `mrkdwn` is already in
/// wire form.
fn text_object(object: &Value) -> Option<String> {
    let text = nonempty(object["text"].as_str())?;
    Some(match object["type"].as_str() {
        Some("plain_text") => escape_entities(text),
        _ => text.to_string(),
    })
}

fn render_image(url: Option<&str>, alt: Option<&str>) -> Option<String> {
    let alt = alt.map(escape_entities);
    match (url, alt) {
        (Some(url), alt) => Some(format!("[image: {}]", render_link(url, alt.as_deref()))),
        (None, Some(alt)) => Some(format!("[image: {alt}]")),
        (None, None) => None,
    }
}

fn image_element(element: &Value) -> Option<String> {
    render_image(
        nonempty(element["image_url"].as_str())
            .or_else(|| nonempty(element["slack_file"]["url"].as_str())),
        nonempty(element["alt_text"].as_str())
            .or_else(|| nonempty(element["title"]["text"].as_str())),
    )
}

/// A link button carries a URL the agent can use; other interactive
/// elements are UI it cannot click.
fn link_button(element: &Value) -> Option<String> {
    let url = nonempty(element["url"].as_str())?;
    Some(render_link(url, text_object(&element["text"]).as_deref()))
}

fn render_block(block: &Value) -> Rendered {
    let Some(kind) = block["type"].as_str() else {
        return Rendered::Silent;
    };
    match kind {
        "header" | "section" => {
            let accessory = &block["accessory"];
            Rendered::from_parts(
                text_object(&block["text"])
                    .into_iter()
                    .chain(as_slice(&block["fields"]).iter().filter_map(text_object))
                    .chain(match accessory["type"].as_str() {
                        Some("image") => image_element(accessory),
                        _ => link_button(accessory),
                    })
                    .collect(),
            )
        }
        "context" => {
            let texts: Vec<String> = as_slice(&block["elements"])
                .iter()
                .filter(|el| matches!(el["type"].as_str(), Some("mrkdwn" | "plain_text")))
                .filter_map(text_object)
                .collect();
            Rendered::from_option((!texts.is_empty()).then(|| texts.join(" · ")))
        }
        "rich_text" => Rendered::from_option(render_rich_text(block)),
        "table" => Rendered::from_parts(as_slice(&block["rows"]).iter().map(render_row).collect()),
        "markdown" => Rendered::from_option(nonempty(block["text"].as_str()).map(escape_entities)),
        "image" => Rendered::from_option(image_element(block)),
        "video" => Rendered::from_option(
            nonempty(block["video_url"].as_str())
                .or_else(|| nonempty(block["title_url"].as_str()))
                .map(|url| {
                    format!(
                        "[video: {}]",
                        render_link(url, text_object(&block["title"]).as_deref())
                    )
                }),
        ),
        "file" => Rendered::from_option(
            nonempty(block["external_id"].as_str())
                .map(|id| format!("[file: {}]", escape_entities(id))),
        ),
        "actions" => {
            let links: Vec<String> = as_slice(&block["elements"])
                .iter()
                .filter_map(link_button)
                .collect();
            Rendered::from_option((!links.is_empty()).then(|| links.join(" · ")))
        }
        "divider" | "input" | "context_actions" => Rendered::Silent,
        unknown => Rendered::Unrendered(join_nonempty(
            std::iter::once(format!(
                "[unrendered Slack {} block]",
                escape_entities(unknown)
            ))
            .chain(harvest_text(block)),
        )),
    }
}

/// Every string held under a `text` key anywhere inside `value` — the best
/// available reading of a block type this renderer has no rule for. Only a
/// `mrkdwn` text object is already in wire form; anything else is literal.
fn harvest_text(value: &Value) -> Vec<String> {
    match value {
        Value::Object(map) => map
            .iter()
            .flat_map(|(key, inner)| match (key.as_str(), inner) {
                ("text", Value::String(text)) => vec![match value["type"].as_str() {
                    Some("mrkdwn") => text.clone(),
                    _ => escape_entities(text),
                }],
                _ => harvest_text(inner),
            })
            .collect(),
        Value::Array(items) => items.iter().flat_map(harvest_text).collect(),
        _ => Vec::new(),
    }
}

fn render_row(row: &Value) -> String {
    let cells: Vec<String> = as_slice(row).iter().map(render_cell).collect();
    format!("| {} |", cells.join(" | "))
}

fn render_cell(cell: &Value) -> String {
    let text = match cell["type"].as_str() {
        Some("rich_text") => render_rich_text(cell).unwrap_or_default(),
        _ => match &cell["text"] {
            Value::String(text) => escape_entities(text),
            Value::Number(number) => number.to_string(),
            _ => String::new(),
        },
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn render_rich_text(container: &Value) -> Option<String> {
    join_parts(
        as_slice(&container["elements"])
            .iter()
            .filter_map(render_rich_text_group)
            .collect(),
    )
}

fn render_rich_text_group(group: &Value) -> Option<String> {
    let inline = |g: &Value, styled: bool| render_rich_inline(as_slice(&g["elements"]), styled);
    match group["type"].as_str()? {
        "rich_text_section" => {
            let text = inline(group, true);
            (!text.trim().is_empty()).then_some(text)
        }
        "rich_text_list" => {
            let ordered = group["style"].as_str() == Some("ordered");
            let indent = "  ".repeat(group["indent"].as_u64().unwrap_or(0).min(8) as usize);
            let first = group["offset"].as_u64().unwrap_or(0) + 1;
            join_parts(
                as_slice(&group["elements"])
                    .iter()
                    .map(|item| inline(item, true))
                    .zip(first..)
                    .map(|(item, number)| match ordered {
                        true => format!("{indent}{number}. {item}"),
                        false => format!("{indent}- {item}"),
                    })
                    .collect(),
            )
        }
        "rich_text_quote" => {
            let text = inline(group, true);
            (!text.trim().is_empty()).then(|| quote(&text))
        }
        "rich_text_preformatted" => {
            let text = inline(group, false);
            (!text.trim().is_empty()).then(|| format!("```\n{text}\n```"))
        }
        _ => None,
    }
}

/// mrkdwn markers for a rich-text style object, innermost first.
const STYLE_MARKERS: [(&str, &str); 4] = [
    ("code", "`"),
    ("bold", "*"),
    ("italic", "_"),
    ("strike", "~"),
];

/// Wrap `text` in the mrkdwn markers its `style` asks for, keeping
/// surrounding whitespace outside them so the markers still bind.
fn apply_style(text: &str, style: &Value) -> String {
    let core = text.trim();
    if core.is_empty() {
        return text.to_string();
    }
    let lead = &text[..text.len() - text.trim_start().len()];
    let trail = &text[text.trim_end().len()..];
    let styled = STYLE_MARKERS
        .iter()
        .filter(|(key, _)| style[*key].as_bool() == Some(true))
        .fold(core.to_string(), |acc, (_, marker)| {
            format!("{marker}{acc}{marker}")
        });
    format!("{lead}{styled}{trail}")
}

fn render_rich_inline(elements: &[Value], styled: bool) -> String {
    let literal = |el: &Value| {
        let text = escape_entities(el["text"].as_str().unwrap_or(""));
        match styled {
            true => apply_style(&text, &el["style"]),
            false => text,
        }
    };
    elements
        .iter()
        .map(|el| match el["type"].as_str().unwrap_or_default() {
            "link" => match nonempty(el["url"].as_str()) {
                Some(url) => render_link(
                    url,
                    nonempty(el["text"].as_str())
                        .filter(|t| *t != url)
                        .map(escape_entities)
                        .as_deref(),
                ),
                None => literal(el),
            },
            "user" => wrap_id(el["user_id"].as_str(), "@"),
            "channel" => wrap_id(el["channel_id"].as_str(), "#"),
            "usergroup" => wrap_id(el["usergroup_id"].as_str(), "!subteam^"),
            "broadcast" => wrap_id(el["range"].as_str(), "!"),
            "emoji" => el["name"]
                .as_str()
                .map(|n| format!(":{n}:"))
                .unwrap_or_default(),
            "date" => nonempty(el["fallback"].as_str())
                .map(escape_entities)
                .or_else(|| el["timestamp"].as_i64().map(|ts| ts.to_string()))
                .unwrap_or_default(),
            "color" => escape_entities(el["value"].as_str().unwrap_or("")),
            _ => literal(el),
        })
        .collect()
}

fn wrap_id(id: Option<&str>, sigil: &str) -> String {
    nonempty(id)
        .map(|id| format!("<{sigil}{id}>"))
        .unwrap_or_default()
}

/// Emit a canonical Slack link token; the final [`compose`] pass rewrites
/// all link tokens to `label (url)` in one place. Raw JSON URLs aren't
/// mrkdwn-escaped: entities are escaped like any literal, and a literal `|`
/// would truncate the token at the label split — `%7C` is equivalent per
/// RFC 3986.
fn render_link(url: &str, label: Option<&str>) -> String {
    let url = escape_entities(url).replace('|', "%7C");
    match label {
        Some(label) => format!("<{url}|{label}>"),
        None => format!("<{url}>"),
    }
}

/// Push `label`, `url`, or a link token combining both — whichever is present.
fn push_labeled_link(parts: &mut Vec<String>, label: Option<&str>, url: Option<&str>) {
    match (label, url) {
        (Some(label), Some(url)) => parts.push(render_link(url, Some(label))),
        (Some(label), None) => parts.push(label.to_string()),
        (None, Some(url)) => parts.push(render_link(url, None)),
        (None, None) => {}
    }
}

/// The `fallback` Slack fills in itself for a block-only attachment.
const NO_PREVIEW_FALLBACK: &str = "[no preview available]";

fn render_attachment(attachment: &Value) -> Option<String> {
    let mut parts = Vec::new();
    push_nonempty(&mut parts, attachment["pretext"].as_str().unwrap_or(""));
    push_labeled_link(
        &mut parts,
        nonempty(attachment["author_name"].as_str()),
        nonempty(attachment["author_link"].as_str()),
    );
    push_labeled_link(
        &mut parts,
        nonempty(attachment["title"].as_str()),
        nonempty(attachment["title_link"].as_str()),
    );
    push_nonempty(&mut parts, attachment["text"].as_str().unwrap_or(""));
    let blocks = render_blocks(as_slice(&attachment["blocks"]).iter());
    parts.extend(blocks.text);
    for field in as_slice(&attachment["fields"]) {
        let line = match (
            nonempty(field["title"].as_str()),
            nonempty(field["value"].as_str()),
        ) {
            (Some(title), Some(value)) => format!("{title}: {value}"),
            (Some(only), None) | (None, Some(only)) => only.to_string(),
            (None, None) => continue,
        };
        parts.push(line);
    }
    push_nonempty(&mut parts, attachment["footer"].as_str().unwrap_or(""));
    // `fallback` duplicates the structured parts; use it only when nothing
    // structured was present or a block could not be rendered.
    if parts.is_empty() || blocks.incomplete {
        parts.extend(
            nonempty(attachment["fallback"].as_str())
                .filter(|fallback| *fallback != NO_PREVIEW_FALLBACK)
                .map(str::to_string),
        );
    }
    join_parts(parts)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn datadog_attachment_renders_title_body_fields_and_footer() {
        let message = json!({
            "bot_id": "B_DATADOG",
            "text": "",
            "attachments": [{
                "fallback": "Recovered: Billing - Consecutive Failures detected",
                "title": "Recovered: Billing - Consecutive Failures detected",
                "title_link": "https://app.datadoghq.eu/monitors/123",
                "text": "Alert has recovered\n<@U_TOM>\n\n<https://app.datadoghq.eu/logs?query=x|View in Log Explorer>",
                "fields": [{"title": "region_code", "value": "WST", "short": true}],
                "footer": "Datadog",
            }],
        });
        assert_eq!(
            compose_message_text(&message),
            "Recovered: Billing - Consecutive Failures detected (https://app.datadoghq.eu/monitors/123)\n\
             Alert has recovered\n<@U_TOM>\n\nView in Log Explorer (https://app.datadoghq.eu/logs?query=x)\n\
             region_code: WST\nDatadog"
        );
    }

    #[test]
    fn attachment_pretext_author_and_title_render_in_display_order() {
        let message = json!({
            "bot_id": "B_GITHUB",
            "text": "",
            "attachments": [{
                "pretext": "New pull request",
                "author_name": "octocat",
                "author_link": "https://github.com/octocat",
                "title": "#42 Fix flaky test",
                "title_link": "https://github.com/x/pull/42",
                "text": "ready for review",
            }],
        });
        assert_eq!(
            compose_message_text(&message),
            "New pull request\noctocat (https://github.com/octocat)\n\
             #42 Fix flaky test (https://github.com/x/pull/42)\nready for review"
        );
    }

    #[test]
    fn section_accessory_link_button_renders() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [{
                "type": "section",
                "text": {"type": "mrkdwn", "text": "PROJ-7 is blocked"},
                "accessory": {
                    "type": "button",
                    "text": {"type": "plain_text", "text": "View issue"},
                    "url": "https://jira.example/browse/PROJ-7",
                },
            }],
        });
        assert_eq!(
            compose_message_text(&message),
            "PROJ-7 is blocked\nView issue (https://jira.example/browse/PROJ-7)"
        );
    }

    #[test]
    fn link_url_containing_pipe_survives_intact() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [{"type": "actions", "elements": [
                {"type": "button", "text": {"type": "plain_text", "text": "View"},
                 "url": "https://app.datadoghq.eu/logs?query=service:(api|worker)"},
            ]}],
        });
        assert_eq!(
            compose_message_text(&message),
            "View (https://app.datadoghq.eu/logs?query=service:(api%7Cworker))"
        );
    }

    #[test]
    fn rich_text_date_renders_fallback() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [{"type": "rich_text", "elements": [
                {"type": "rich_text_section", "elements": [
                    {"type": "text", "text": "due "},
                    {"type": "date", "timestamp": 1752192000i64, "format": "{date_short}",
                     "fallback": "Jul 11, 2026"},
                ]},
            ]}],
        });
        assert_eq!(compose_message_text(&message), "due Jul 11, 2026");
    }

    #[test]
    fn attachment_falls_back_to_fallback_only_when_unstructured() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "attachments": [{"fallback": "plain fallback body"}],
        });
        assert_eq!(compose_message_text(&message), "plain fallback body");
    }

    #[test]
    fn attachment_nested_blocks_win_over_fallback() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "attachments": [{
                "fallback": "should not appear",
                "blocks": [
                    {"type": "section", "text": {"type": "mrkdwn", "text": "*CPU high* on host-1"}},
                ],
            }],
        });
        assert_eq!(compose_message_text(&message), "*CPU high* on host-1");
    }

    #[test]
    fn blocks_replace_notification_fallback_text() {
        let message = json!({
            "bot_id": "B_SELF",
            "text": "Deploy finished (notification fallback)",
            "blocks": [
                {"type": "section", "text": {"type": "mrkdwn", "text": "Deploy finished: all 12 tasks healthy"}},
            ],
        });
        assert_eq!(
            compose_message_text(&message),
            "Deploy finished: all 12 tasks healthy"
        );
    }

    #[test]
    fn empty_block_render_falls_back_to_text() {
        let message = json!({
            "bot_id": "B1",
            "text": "only the fallback survives",
            "blocks": [{"type": "divider"}],
        });
        assert_eq!(compose_message_text(&message), "only the fallback survives");
    }

    #[test]
    fn header_and_section_fields_render() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [
                {"type": "header", "text": {"type": "plain_text", "text": "Incident P1"}},
                {"type": "section", "fields": [
                    {"type": "mrkdwn", "text": "*Service:* billing"},
                    {"type": "mrkdwn", "text": "*Env:* prod"},
                ]},
            ],
        });
        assert_eq!(
            compose_message_text(&message),
            "Incident P1\n*Service:* billing\n*Env:* prod"
        );
    }

    #[test]
    fn context_elements_join_and_skip_images() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [{"type": "context", "elements": [
                {"type": "image", "image_url": "https://x/avatar.png", "alt_text": "avatar"},
                {"type": "mrkdwn", "text": "triggered by monitor 42"},
                {"type": "plain_text", "text": "eu-west-2"},
            ]}],
        });
        assert_eq!(
            compose_message_text(&message),
            "triggered by monitor 42 · eu-west-2"
        );
    }

    #[test]
    fn rich_text_inline_tokens_render_slack_syntax() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [{"type": "rich_text", "elements": [
                {"type": "rich_text_section", "elements": [
                    {"type": "text", "text": "ping "},
                    {"type": "user", "user_id": "U42"},
                    {"type": "text", "text": " in "},
                    {"type": "channel", "channel_id": "C42"},
                    {"type": "text", "text": " see "},
                    {"type": "link", "url": "https://runbook.example", "text": "runbook"},
                    {"type": "text", "text": " "},
                    {"type": "emoji", "name": "fire"},
                    {"type": "text", "text": " "},
                    {"type": "broadcast", "range": "here"},
                ]},
            ]}],
        });
        assert_eq!(
            compose_message_text(&message),
            "ping <@U42> in <#C42> see runbook (https://runbook.example) :fire: <!here>"
        );
    }

    #[test]
    fn rich_text_list_quote_and_preformatted_render() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [{"type": "rich_text", "elements": [
                {"type": "rich_text_list", "style": "ordered", "elements": [
                    {"type": "rich_text_section", "elements": [{"type": "text", "text": "first"}]},
                    {"type": "rich_text_section", "elements": [{"type": "text", "text": "second"}]},
                ]},
                {"type": "rich_text_list", "style": "bullet", "elements": [
                    {"type": "rich_text_section", "elements": [{"type": "text", "text": "point"}]},
                ]},
                {"type": "rich_text_quote", "elements": [{"type": "text", "text": "a\nb"}]},
                {"type": "rich_text_preformatted", "elements": [{"type": "text", "text": "let x = 1;"}]},
            ]}],
        });
        assert_eq!(
            compose_message_text(&message),
            "1. first\n2. second\n- point\n> a\n> b\n```\nlet x = 1;\n```"
        );
    }

    #[test]
    fn actions_render_only_link_buttons() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [{"type": "actions", "elements": [
                {"type": "button", "text": {"type": "plain_text", "text": "View in Log Explorer"},
                 "url": "https://app.datadoghq.eu/logs?query=x"},
                {"type": "button", "text": {"type": "plain_text", "text": "Acknowledge"},
                 "action_id": "ack"},
            ]}],
        });
        assert_eq!(
            compose_message_text(&message),
            "View in Log Explorer (https://app.datadoghq.eu/logs?query=x)"
        );
    }

    #[test]
    fn human_message_with_unfurl_attachments_is_untouched() {
        let message = json!({
            "user": "U1",
            "text": "look at https://example.com",
            "attachments": [{
                "title": "Example Domain",
                "text": "unfurl preview",
                "from_url": "https://example.com",
                "original_url": "https://example.com",
            }],
        });
        assert_eq!(
            compose_message_text(&message),
            "look at https://example.com"
        );
    }

    #[test]
    fn human_share_forward_renders_quoted_content() {
        let message = json!({
            "user": "U1",
            "text": "what happened here?",
            "attachments": [{
                "is_share": true,
                "author_name": "Datadog",
                "channel_name": "monitoring-interop",
                "text": "Triggered: CPU high on host-1",
                "ts": "99.9",
            }],
        });
        assert_eq!(
            compose_message_text(&message),
            "what happened here?\n[shared message from Datadog in #monitoring-interop]\n\
             > Triggered: CPU high on host-1"
        );
    }

    #[test]
    fn share_with_message_blocks_only_renders_their_content() {
        let message = json!({
            "user": "U1",
            "text": "",
            "attachments": [{
                "is_msg_unfurl": true,
                "author_name": "Kingsley",
                "message_blocks": {"message": {"blocks": [
                    {"type": "rich_text", "elements": [
                        {"type": "rich_text_section", "elements": [
                            {"type": "text", "text": "deploy is done"},
                        ]},
                    ]},
                ]}},
            }],
        });
        assert_eq!(
            compose_message_text(&message),
            "[shared message from Kingsley]\n> deploy is done"
        );
    }

    #[test]
    fn bot_share_attachment_uses_share_framing_not_generic_fields() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "attachments": [{
                "is_share": true,
                "author_name": "Sam Carter",
                "text": "escalating this",
                "footer": "Slack Conversation",
            }],
        });
        assert_eq!(
            compose_message_text(&message),
            "[shared message from Sam Carter]\n> escalating this"
        );
    }

    #[test]
    fn nested_rich_text_lists_indent() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [{"type": "rich_text", "elements": [
                {"type": "rich_text_list", "style": "bullet", "elements": [
                    {"type": "rich_text_section", "elements": [{"type": "text", "text": "outer"}]},
                ]},
                {"type": "rich_text_list", "style": "bullet", "indent": 1, "elements": [
                    {"type": "rich_text_section", "elements": [{"type": "text", "text": "inner"}]},
                ]},
            ]}],
        });
        assert_eq!(compose_message_text(&message), "- outer\n  - inner");
    }

    #[test]
    fn bot_message_subtype_counts_as_bot_without_bot_id() {
        let message = json!({
            "subtype": "bot_message",
            "text": "",
            "attachments": [{"fallback": "webhook body"}],
        });
        assert_eq!(compose_message_text(&message), "webhook body");
    }

    #[test]
    fn bot_text_and_attachment_both_render() {
        let message = json!({
            "bot_id": "B1",
            "text": "heads up",
            "attachments": [{"text": "details here"}],
        });
        assert_eq!(compose_message_text(&message), "heads up\ndetails here");
    }

    #[test]
    fn empty_bot_message_stays_empty() {
        let message = json!({"bot_id": "B1", "text": ""});
        assert_eq!(compose_message_text(&message), "");
    }

    fn pasted_table() -> Value {
        json!({
            "id": 1,
            "fallback": "[no preview available]",
            "blocks": [{"type": "table", "rows": [
                [
                    {"type": "rich_text", "elements": [{"type": "rich_text_section", "elements": [
                        {"type": "text", "text": "Email", "style": {"bold": true}},
                    ]}]},
                    {"type": "raw_text", "text": "Status"},
                ],
                [
                    {"type": "rich_text", "elements": [{"type": "rich_text_section", "elements": [
                        {"type": "link", "url": "mailto:a.b@example.org", "text": "a.b@example.org"},
                    ]}]},
                    {"type": "raw_text", "text": "Failed"},
                ],
            ]}],
        })
    }

    #[test]
    fn human_pasted_table_attachment_renders_rows() {
        let message = json!({
            "user": "U1",
            "text": "these users are getting this error:",
            "blocks": [{"type": "rich_text", "elements": [{"type": "rich_text_section", "elements": [
                {"type": "text", "text": "these users are getting this error:"},
            ]}]}],
            "attachments": [pasted_table()],
        });
        assert_eq!(
            compose_message_text(&message),
            "these users are getting this error:\n\
             | *Email* | Status |\n\
             | a.b@example.org | Failed |"
        );
    }

    #[test]
    fn human_top_level_table_block_renders_rows() {
        let message = json!({
            "user": "U1",
            "text": "see below",
            "blocks": [
                {"type": "rich_text", "elements": [{"type": "rich_text_section", "elements": [
                    {"type": "text", "text": "see below"},
                ]}]},
                {"type": "table", "rows": [[
                    {"type": "raw_text", "text": "a"},
                    {"type": "raw_number", "text": 7},
                ]]},
            ],
        });
        assert_eq!(compose_message_text(&message), "see below\n| a | 7 |");
    }

    #[test]
    fn human_block_only_message_renders_rich_text() {
        let message = json!({
            "user": "U1",
            "text": "",
            "blocks": [{"type": "rich_text", "elements": [{"type": "rich_text_section", "elements": [
                {"type": "text", "text": "only in blocks"},
            ]}]}],
        });
        assert_eq!(compose_message_text(&message), "only in blocks");
    }

    #[test]
    fn human_typed_link_keeps_url_and_autolink_reads_as_typed() {
        let message = json!({
            "user": "U1",
            "text": "see <https://example.com/runbook|the runbook> or mail <mailto:ops@example.com|ops@example.com>",
        });
        assert_eq!(
            compose_message_text(&message),
            "see the runbook (https://example.com/runbook) or mail ops@example.com"
        );
    }

    #[test]
    fn human_app_unfurl_is_excluded_but_share_with_from_url_is_kept() {
        let message = json!({
            "user": "U1",
            "text": "two links",
            "attachments": [
                {"is_app_unfurl": true, "app_unfurl_url": "https://tracker.example/1",
                 "blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": "issue preview"}}]},
                {"is_msg_unfurl": true, "from_url": "https://x.slack.com/archives/C1/p1",
                 "author_name": "Sam", "text": "the quoted message"},
            ],
        });
        assert_eq!(
            compose_message_text(&message),
            "two links\n[shared message from Sam]\n> the quoted message"
        );
    }

    #[test]
    fn shares_are_kept_apart_from_the_body() {
        let content = compose(
            "what happened here?",
            false,
            &Value::Null,
            &json!([{"is_share": true, "author_name": "Alerts", "text": "CPU high"}]),
        );
        assert_eq!(content.body, "what happened here?");
        assert_eq!(content.shared, ["[shared message from Alerts]\n> CPU high"]);
    }

    #[test]
    fn share_without_text_lists_files_or_says_so() {
        let message = json!({
            "user": "U1",
            "text": "",
            "attachments": [
                {"is_share": true, "author_name": "Sam", "files": [{"name": "report.pdf"}]},
                {"is_share": true, "author_name": "Kim"},
            ],
        });
        assert_eq!(
            compose_message_text(&message),
            "[shared message from Sam]\n> [file: report.pdf]\n\
             [shared message from Kim]\n> [no readable content]"
        );
    }

    #[test]
    fn unknown_block_emits_marker_and_keeps_text_fallback() {
        let message = json!({
            "bot_id": "B1",
            "text": "Quarterly numbers: up 4%",
            "blocks": [{"type": "data_visualization", "title": {"type": "plain_text", "text": "Revenue"}}],
        });
        assert_eq!(
            compose_message_text(&message),
            "[unrendered Slack data_visualization block]\nRevenue\nQuarterly numbers: up 4%"
        );
    }

    #[test]
    fn bot_partial_block_render_keeps_text_fallback() {
        let message = json!({
            "bot_id": "B1",
            "text": "Deploy 42 failed in stage 3",
            "blocks": [
                {"type": "context", "elements": [{"type": "mrkdwn", "text": "ci-bot"}]},
                {"type": "task_card"},
            ],
        });
        assert_eq!(
            compose_message_text(&message),
            "ci-bot\n[unrendered Slack task_card block]\nDeploy 42 failed in stage 3"
        );
    }

    #[test]
    fn human_unknown_block_emits_marker_after_text() {
        let message = json!({
            "user": "U1",
            "text": "poll",
            "blocks": [{"type": "carousel"}],
        });
        assert_eq!(
            compose_message_text(&message),
            "poll\n[unrendered Slack carousel block]"
        );
    }

    #[test]
    fn rich_text_styles_render_as_mrkdwn() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [{"type": "rich_text", "elements": [
                {"type": "rich_text_section", "elements": [
                    {"type": "text", "text": "run "},
                    {"type": "text", "text": "cargo test", "style": {"code": true}},
                    {"type": "text", "text": " then "},
                    {"type": "text", "text": "ship ", "style": {"bold": true, "italic": true}},
                    {"type": "text", "text": "never", "style": {"strike": true}},
                ]},
                {"type": "rich_text_preformatted", "elements": [
                    {"type": "text", "text": "a*b", "style": {"bold": true}},
                ]},
            ]}],
        });
        assert_eq!(
            compose_message_text(&message),
            "run `cargo test` then _*ship*_ ~never~\n```\na*b\n```"
        );
    }

    #[test]
    fn ordered_list_honours_offset() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [{"type": "rich_text", "elements": [
                {"type": "rich_text_list", "style": "ordered", "offset": 2, "elements": [
                    {"type": "rich_text_section", "elements": [{"type": "text", "text": "third"}]},
                    {"type": "rich_text_section", "elements": [{"type": "text", "text": "fourth"}]},
                ]},
            ]}],
        });
        assert_eq!(compose_message_text(&message), "3. third\n4. fourth");
    }

    #[test]
    fn literal_angle_brackets_are_not_tokens() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [{"type": "rich_text", "elements": [
                {"type": "rich_text_preformatted", "elements": [
                    {"type": "text", "text": "echo <https://example.com|click> && <@U1>"},
                ]},
            ]}],
        });
        let composed = compose_message_text(&message);
        assert_eq!(
            composed,
            "```\necho &lt;https://example.com|click&gt; &amp;&amp; &lt;@U1&gt;\n```"
        );
        let names = HashMap::from([("U1".to_string(), "Sam".to_string())]);
        assert_eq!(
            finalize_inbound_text(&composed, &names, &HashMap::new()),
            "```\necho &lt;https://example.com|click> && &lt;@U1>\n```"
        );
    }

    #[test]
    fn image_video_file_and_markdown_blocks_render() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [
                {"type": "image", "image_url": "https://img.example/a.png", "alt_text": "latency graph"},
                {"type": "video", "video_url": "https://v.example/1",
                 "title": {"type": "plain_text", "text": "Demo"}},
                {"type": "file", "external_id": "ABCD1", "source": "remote"},
                {"type": "markdown", "text": "## Heading\n- item"},
                {"type": "section", "text": {"type": "mrkdwn", "text": "with picture"},
                 "accessory": {"type": "image", "image_url": "https://img.example/b.png", "alt_text": "logo"}},
                {"type": "divider"},
                {"type": "input"},
            ],
        });
        assert_eq!(
            compose_message_text(&message),
            "[image: latency graph (https://img.example/a.png)]\n\
             [video: Demo (https://v.example/1)]\n\
             [file: ABCD1]\n\
             ## Heading\n- item\n\
             with picture\n[image: logo (https://img.example/b.png)]"
        );
    }

    #[test]
    fn finalized_content_shows_names_with_their_ids() {
        let content = compose(
            "ask <!subteam^S1|@platform> or <@U7> in <#C1|general> about a &lt; b &amp;&amp; c",
            false,
            &Value::Null,
            &Value::Null,
        )
        .finalized();
        assert_eq!(
            content.into_text(),
            "ask @platform (S1) or <@U7> in #general (C1) about a < b && c"
        );
    }

    #[test]
    fn forwarded_alert_renders_blocks_from_either_shape() {
        let message_blocks = json!({"message": {"blocks": [
            {"type": "section", "text": {"type": "mrkdwn", "text": "CPU 97% on host-1"}},
        ]}});
        for shape in [message_blocks.clone(), json!([message_blocks])] {
            let message = json!({
                "user": "U1",
                "text": "",
                "attachments": [{
                    "is_share": true,
                    "author_name": "Alerts <prod>",
                    "text": "Alert fired",
                    "message_blocks": shape,
                    "files": [{"title": "graph.png"}],
                }],
            });
            assert_eq!(
                compose_message_text(&message),
                "[shared message from Alerts &lt;prod&gt;]\n> CPU 97% on host-1\n> [file: graph.png]"
            );
        }
    }

    #[test]
    fn unknown_block_harvest_escapes_literal_text() {
        let message = json!({
            "user": "U1",
            "text": "see card",
            "blocks": [{"type": "card",
                "title": {"type": "plain_text", "text": "<!channel> a < b"},
                "body": {"type": "mrkdwn", "text": "ask <@U2>"}}],
        });
        assert_eq!(
            compose_message_text(&message),
            "see card\n[unrendered Slack card block]\nask <@U2>\n&lt;!channel&gt; a &lt; b"
        );
    }

    #[test]
    fn empty_block_attachment_does_not_surface_placeholder_fallback() {
        let message = json!({
            "user": "U1",
            "text": "",
            "attachments": [{"id": 1, "fallback": "[no preview available]",
                "blocks": [{"type": "table", "rows": []}]}],
        });
        assert_eq!(compose_message_text(&message), "");
    }

    #[test]
    fn link_url_with_entities_survives_finalize() {
        let message = json!({
            "bot_id": "B1",
            "text": "",
            "blocks": [{"type": "rich_text", "elements": [{"type": "rich_text_section", "elements": [
                {"type": "link", "url": "https://x.example/q?a=1&lt=2&amp;b", "text": "query"},
            ]}]}],
        });
        let none = HashMap::new();
        assert_eq!(
            finalize_inbound_text(&compose_message_text(&message), &none, &none),
            "query (https://x.example/q?a=1&lt=2&amp;b)"
        );
    }
}
