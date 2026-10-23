//! Render agent-authored markdown into Atlassian Document Format (ADF).
//!
//! Jira REST v3 requires `description` and comment `body` fields to be ADF
//! JSON. Our agents emit markdown by convention (matching Linear, Slack,
//! GitHub, and our own SKILL.md files), so this module parses their markdown
//! and builds ADF nodes for the subset they use. Unsupported constructs
//! degrade gracefully rather than failing the request.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde_json::{json, Value};
use tracing::warn;

/// Convert a markdown string into an ADF document.
///
/// Empty or whitespace-only input yields an empty doc. Any input that survives
/// parsing always produces a valid `{type:"doc", version:1, content:[...]}`
/// structure — even if the parser state goes off the rails internally, the
/// function falls back to wrapping the original text in a single paragraph so
/// the Jira request still succeeds.
pub(super) fn markdown_to_adf(s: &str) -> Value {
    if s.trim().is_empty() {
        return empty_doc();
    }

    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(s, opts);

    let mut visitor = Visitor::new();
    for event in parser {
        visitor.handle(event);
    }
    visitor.finish(s)
}

fn empty_doc() -> Value {
    json!({ "type": "doc", "version": 1, "content": [] })
}

/// ADF rejects repeated marks of one type on a node (nested same-type
/// emphasis, an image inside a link); keep the first of each type.
fn dedup_marks(marks: &[Value]) -> Vec<Value> {
    let mut seen: Vec<&str> = Vec::new();
    marks
        .iter()
        .filter(|m| {
            let ty = m["type"].as_str().unwrap_or("");
            let fresh = !seen.contains(&ty);
            if fresh {
                seen.push(ty);
            }
            fresh
        })
        .cloned()
        .collect()
}

fn fallback_doc(original: &str) -> Value {
    json!({
        "type": "doc",
        "version": 1,
        "content": [{
            "type": "paragraph",
            "content": [{ "type": "text", "text": original }]
        }]
    })
}

#[derive(Clone, Debug, PartialEq)]
enum BlockKind {
    Doc,
    Paragraph,
    /// Auto-opened paragraph used to wrap inline content in places where ADF
    /// requires a block parent (tight list items). Closed automatically at
    /// block-level transitions.
    ImplicitParagraph,
    Heading {
        level: u32,
    },
    BlockQuote,
    CodeBlock {
        language: Option<String>,
    },
    BulletList,
    OrderedList,
    ListItem,
}

impl BlockKind {
    fn accepts_inline(&self) -> bool {
        matches!(
            self,
            BlockKind::Paragraph
                | BlockKind::ImplicitParagraph
                | BlockKind::Heading { .. }
                | BlockKind::CodeBlock { .. }
        )
    }
}

struct Block {
    kind: BlockKind,
    content: Vec<Value>,
}

struct Visitor {
    blocks: Vec<Block>,
    marks: Vec<Value>,
    /// Nesting depth inside a markdown table. Tables aren't rendered natively;
    /// all inner events are swallowed and replaced with a single placeholder
    /// paragraph when the outermost table closes.
    table_depth: u32,
}

impl Visitor {
    fn new() -> Self {
        Self {
            blocks: vec![Block {
                kind: BlockKind::Doc,
                content: Vec::new(),
            }],
            marks: Vec::new(),
            table_depth: 0,
        }
    }

    fn handle(&mut self, event: Event) {
        if self.table_depth > 0 {
            match event {
                Event::Start(Tag::Table(_)) => self.table_depth += 1,
                Event::End(TagEnd::Table) => {
                    self.table_depth -= 1;
                    if self.table_depth == 0 {
                        self.attach_to_current(json!({
                            "type": "paragraph",
                            "content": [{
                                "type": "text",
                                "text": "[unsupported: table — pass raw ADF for table content]"
                            }]
                        }));
                    }
                }
                _ => {}
            }
            return;
        }

        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(end) => self.end_tag(end),
            Event::Text(text) => self.emit_text(text.as_ref()),
            Event::Code(text) => self.emit_inline_code(text.as_ref()),
            Event::SoftBreak => self.emit_text(" "),
            Event::HardBreak => self.emit_inline(json!({ "type": "hardBreak" })),
            Event::Rule => {
                self.close_implicit_paragraph();
                self.attach_to_current(json!({ "type": "rule" }));
            }
            Event::Html(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
    }

    fn start_tag(&mut self, tag: Tag) {
        let is_inline = matches!(
            &tag,
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } | Tag::Image { .. }
        );
        if !is_inline {
            self.close_implicit_paragraph();
        }

        match tag {
            Tag::Paragraph => self.push_block(BlockKind::Paragraph),
            Tag::Heading { level, .. } => {
                self.push_block(BlockKind::Heading {
                    level: level as u32,
                });
            }
            Tag::BlockQuote(_) => self.push_block(BlockKind::BlockQuote),
            Tag::CodeBlock(kind) => {
                let language = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => Some(lang.to_string()),
                    CodeBlockKind::Fenced(_) | CodeBlockKind::Indented => None,
                };
                self.push_block(BlockKind::CodeBlock { language });
            }
            Tag::List(start) => self.push_block(if start.is_some() {
                BlockKind::OrderedList
            } else {
                BlockKind::BulletList
            }),
            Tag::Item => self.push_block(BlockKind::ListItem),
            Tag::Emphasis => self.marks.push(json!({ "type": "em" })),
            Tag::Strong => self.marks.push(json!({ "type": "strong" })),
            Tag::Strikethrough => self.marks.push(json!({ "type": "strike" })),
            // Images fall back to a hyperlinked alt-text span because ADF
            // `media` nodes require an uploaded media ID we don't have.
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.push_link_mark(&dest_url);
            }
            Tag::Table(_) => self.table_depth = 1,
            Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition => {}
        }
    }

    fn push_link_mark(&mut self, href: &str) {
        self.marks.push(json!({
            "type": "link",
            "attrs": { "href": href }
        }));
    }

    fn end_tag(&mut self, end: TagEnd) {
        if matches!(end, TagEnd::Item | TagEnd::BlockQuote(_) | TagEnd::List(_)) {
            self.close_implicit_paragraph();
        }

        match end {
            TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::List(_)
            | TagEnd::Item => self.pop_block(),
            TagEnd::Emphasis => self.pop_mark("em"),
            TagEnd::Strong => self.pop_mark("strong"),
            TagEnd::Strikethrough => self.pop_mark("strike"),
            TagEnd::Link | TagEnd::Image => self.pop_mark("link"),
            TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition => {}
        }
    }

    fn emit_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.ensure_inline_container();
        // Code blocks hold plain text with no marks — ADF doesn't allow them there.
        let in_code_block = matches!(self.current_kind(), BlockKind::CodeBlock { .. });
        let marks = dedup_marks(&self.marks);
        let node = if in_code_block || marks.is_empty() {
            json!({ "type": "text", "text": text })
        } else {
            json!({ "type": "text", "text": text, "marks": marks })
        };
        self.attach_to_current(node);
    }

    fn emit_inline_code(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.ensure_inline_container();
        // ADF permits `code` to combine only with `link` — any other active
        // mark (em/strong/strike) on a code span makes Jira reject the whole
        // document, so the illegal ones are dropped for the span alone.
        let links: Vec<Value> = self
            .marks
            .iter()
            .filter(|m| m["type"] == "link")
            .cloned()
            .collect();
        let mut marks = dedup_marks(&links);
        marks.push(json!({ "type": "code" }));
        self.attach_to_current(json!({
            "type": "text",
            "text": text,
            "marks": marks
        }));
    }

    fn emit_inline(&mut self, node: Value) {
        self.ensure_inline_container();
        self.attach_to_current(node);
    }

    fn ensure_inline_container(&mut self) {
        if !self.current_kind().accepts_inline() {
            self.push_block(BlockKind::ImplicitParagraph);
        }
    }

    fn close_implicit_paragraph(&mut self) {
        if matches!(self.current_kind(), BlockKind::ImplicitParagraph) {
            self.pop_block();
        }
    }

    fn current_kind(&self) -> &BlockKind {
        &self.blocks.last().expect("block stack never empty").kind
    }

    fn push_block(&mut self, kind: BlockKind) {
        self.blocks.push(Block {
            kind,
            content: Vec::new(),
        });
    }

    fn pop_block(&mut self) {
        if self.blocks.len() <= 1 {
            warn!("markdown_to_adf: attempted to pop the root doc — parser state corrupt");
            return;
        }
        let block = self.blocks.pop().expect("length > 1 checked above");
        let node = block_to_value(block);
        self.attach_to_current(node);
    }

    fn attach_to_current(&mut self, node: Value) {
        if let Some(top) = self.blocks.last_mut() {
            top.content.push(node);
        }
    }

    fn pop_mark(&mut self, ty: &str) {
        if let Some(pos) = self.marks.iter().rposition(|m| m["type"] == ty) {
            self.marks.remove(pos);
        }
    }

    fn finish(mut self, original: &str) -> Value {
        while self.blocks.len() > 1 {
            self.pop_block();
        }
        let Some(doc) = self.blocks.pop() else {
            return fallback_doc(original);
        };
        if !matches!(doc.kind, BlockKind::Doc) {
            warn!("markdown_to_adf: root stack frame is not Doc after drain");
            return fallback_doc(original);
        }
        block_to_value(doc)
    }
}

fn block_to_value(block: Block) -> Value {
    match block.kind {
        BlockKind::Doc => json!({
            "type": "doc",
            "version": 1,
            "content": block.content,
        }),
        BlockKind::Paragraph | BlockKind::ImplicitParagraph => json!({
            "type": "paragraph",
            "content": block.content,
        }),
        BlockKind::Heading { level } => json!({
            "type": "heading",
            "attrs": { "level": level },
            "content": block.content,
        }),
        BlockKind::BlockQuote => json!({
            "type": "blockquote",
            "content": block.content,
        }),
        BlockKind::CodeBlock { language } => match language {
            Some(lang) => json!({
                "type": "codeBlock",
                "attrs": { "language": lang },
                "content": block.content,
            }),
            None => json!({
                "type": "codeBlock",
                "content": block.content,
            }),
        },
        BlockKind::BulletList => json!({
            "type": "bulletList",
            "content": block.content,
        }),
        BlockKind::OrderedList => json!({
            "type": "orderedList",
            "content": block.content,
        }),
        BlockKind::ListItem => json!({
            "type": "listItem",
            "content": block.content,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark_types(text_node: &Value) -> Vec<&str> {
        text_node["marks"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|m| m["type"].as_str()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn duplicate_marks_of_one_type_are_deduped() {
        // Nested same-type emphasis (`*_x_*`) must not stack two `em` marks.
        let v = markdown_to_adf("*_x_*");
        assert_eq!(mark_types(&v["content"][0]["content"][0]), vec!["em"]);
    }

    #[test]
    fn inline_code_keeps_only_legal_marks() {
        // ADF: `code` may combine only with `link` — em/strong/strike on a
        // code span must be dropped for the span alone.
        let v = markdown_to_adf("_Fingerprint: `errfp-bac1b67f1013` tail_");
        let para = &v["content"][0]["content"];
        assert_eq!(mark_types(&para[0]), vec!["em"], "leading italic text");
        assert_eq!(mark_types(&para[1]), vec!["code"], "code span sheds em");
        assert_eq!(para[1]["text"], "errfp-bac1b67f1013");
        assert_eq!(mark_types(&para[2]), vec!["em"], "trailing italic text");

        let v = markdown_to_adf("**~~`x`~~**");
        let code = &v["content"][0]["content"][0];
        assert_eq!(mark_types(code), vec!["code"]);
    }

    #[test]
    fn inline_code_inside_a_link_keeps_the_link_mark() {
        let v = markdown_to_adf("[`docs`](https://example.com)");
        let code = &v["content"][0]["content"][0];
        let marks = mark_types(code);
        assert_eq!(marks, vec!["link", "code"]);
    }

    #[test]
    fn empty_string_produces_empty_doc() {
        let v = markdown_to_adf("");
        assert_eq!(v, json!({ "type": "doc", "version": 1, "content": [] }));
    }

    #[test]
    fn whitespace_only_produces_empty_doc() {
        let v = markdown_to_adf("   \n\n   ");
        assert_eq!(v["content"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn plain_paragraph_has_no_marks_field() {
        let v = markdown_to_adf("hello world");
        let text = &v["content"][0]["content"][0];
        assert_eq!(v["type"], "doc");
        assert_eq!(v["version"], 1);
        assert_eq!(v["content"][0]["type"], "paragraph");
        assert_eq!(text["type"], "text");
        assert_eq!(text["text"], "hello world");
        // ADF validators reject empty marks arrays — ensure we omit the field.
        assert!(text.get("marks").is_none());
    }

    #[test]
    fn heading_levels_h1_through_h6() {
        for (md, level) in [
            ("# h1", 1),
            ("## h2", 2),
            ("### h3", 3),
            ("#### h4", 4),
            ("##### h5", 5),
            ("###### h6", 6),
        ] {
            let v = markdown_to_adf(md);
            assert_eq!(v["content"][0]["type"], "heading", "md: {md}");
            assert_eq!(v["content"][0]["attrs"]["level"], level, "md: {md}");
        }
    }

    #[test]
    fn bold_mark() {
        let v = markdown_to_adf("**bold**");
        let text = &v["content"][0]["content"][0];
        assert_eq!(text["text"], "bold");
        assert_eq!(mark_types(text), vec!["strong"]);
    }

    #[test]
    fn italic_mark() {
        let v = markdown_to_adf("_italic_");
        let text = &v["content"][0]["content"][0];
        assert_eq!(text["text"], "italic");
        assert_eq!(mark_types(text), vec!["em"]);
    }

    #[test]
    fn bold_italic_combined() {
        let v = markdown_to_adf("***both***");
        let text = &v["content"][0]["content"][0];
        assert_eq!(text["text"], "both");
        let marks = mark_types(text);
        assert!(marks.contains(&"strong"), "marks: {:?}", marks);
        assert!(marks.contains(&"em"), "marks: {:?}", marks);
    }

    #[test]
    fn inline_code_mark() {
        let v = markdown_to_adf("`foo`");
        let text = &v["content"][0]["content"][0];
        assert_eq!(text["text"], "foo");
        assert_eq!(mark_types(text), vec!["code"]);
    }

    #[test]
    fn strikethrough_mark() {
        let v = markdown_to_adf("~~gone~~");
        let text = &v["content"][0]["content"][0];
        assert_eq!(text["text"], "gone");
        assert_eq!(mark_types(text), vec!["strike"]);
    }

    #[test]
    fn link_mark() {
        let v = markdown_to_adf("[label](https://example.com)");
        let text = &v["content"][0]["content"][0];
        assert_eq!(text["text"], "label");
        assert_eq!(text["marks"][0]["type"], "link");
        assert_eq!(text["marks"][0]["attrs"]["href"], "https://example.com");
    }

    #[test]
    fn link_containing_inline_code_has_both_marks() {
        let v = markdown_to_adf("[`foo`](https://x)");
        let text = &v["content"][0]["content"][0];
        assert_eq!(text["text"], "foo");
        let marks = mark_types(text);
        assert!(marks.contains(&"code"), "marks: {:?}", marks);
        assert!(marks.contains(&"link"), "marks: {:?}", marks);
    }

    #[test]
    fn fenced_code_with_language() {
        let v = markdown_to_adf("```rust\nfn main() {}\n```");
        let block = &v["content"][0];
        assert_eq!(block["type"], "codeBlock");
        assert_eq!(block["attrs"]["language"], "rust");
        assert_eq!(block["content"][0]["type"], "text");
        assert!(block["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("fn main()"));
        // No marks allowed inside codeBlock.
        assert!(block["content"][0].get("marks").is_none());
    }

    #[test]
    fn fenced_code_without_language_has_no_language_attr() {
        let v = markdown_to_adf("```\nplain\n```");
        let block = &v["content"][0];
        assert_eq!(block["type"], "codeBlock");
        let has_lang = block.get("attrs").and_then(|a| a.get("language")).is_some();
        assert!(!has_lang, "block: {block}");
    }

    #[test]
    fn bullet_list_items_wrap_text_in_paragraphs() {
        let v = markdown_to_adf("- one\n- two");
        let list = &v["content"][0];
        assert_eq!(list["type"], "bulletList");
        let items = list["content"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "listItem");
        // Tight-list items auto-wrap in paragraph for ADF validity.
        assert_eq!(items[0]["content"][0]["type"], "paragraph");
        assert_eq!(items[0]["content"][0]["content"][0]["text"], "one");
        assert_eq!(items[1]["content"][0]["content"][0]["text"], "two");
    }

    #[test]
    fn ordered_list() {
        let v = markdown_to_adf("1. first\n2. second");
        let list = &v["content"][0];
        assert_eq!(list["type"], "orderedList");
        assert_eq!(list["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn nested_list() {
        let v = markdown_to_adf("- outer\n  - inner");
        let outer = &v["content"][0];
        assert_eq!(outer["type"], "bulletList");
        let outer_item = &outer["content"][0];
        // Outer item: paragraph + nested bulletList
        let inner_list = outer_item["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["type"] == "bulletList")
            .expect("nested bulletList");
        assert_eq!(
            inner_list["content"][0]["content"][0]["content"][0]["text"],
            "inner"
        );
    }

    #[test]
    fn blockquote_with_two_paragraphs() {
        let v = markdown_to_adf("> quoted\n>\n> twice");
        let bq = &v["content"][0];
        assert_eq!(bq["type"], "blockquote");
        let paras = bq["content"].as_array().unwrap();
        assert_eq!(paras.len(), 2);
        assert_eq!(paras[0]["type"], "paragraph");
    }

    #[test]
    fn hard_break_emits_hardbreak_node() {
        let v = markdown_to_adf("line one  \nline two");
        let p = &v["content"][0];
        let types: Vec<&str> = p["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["type"].as_str().unwrap())
            .collect();
        assert!(types.contains(&"hardBreak"), "types: {:?}", types);
    }

    #[test]
    fn soft_break_joins_with_space() {
        let v = markdown_to_adf("line one\nline two");
        let p = &v["content"][0];
        let joined: String = p["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["text"].as_str())
            .collect();
        assert!(joined.contains("line one line two"), "joined: {joined:?}");
    }

    #[test]
    fn table_falls_back_to_placeholder() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |";
        let v = markdown_to_adf(md);
        let first = &v["content"][0];
        assert_eq!(first["type"], "paragraph");
        assert!(first["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unsupported: table"));
    }

    #[test]
    fn image_renders_as_link_with_alt_text() {
        let v = markdown_to_adf("![alt](https://img/example.png)");
        let text = &v["content"][0]["content"][0];
        assert_eq!(text["text"], "alt");
        assert_eq!(text["marks"][0]["type"], "link");
        assert_eq!(text["marks"][0]["attrs"]["href"], "https://img/example.png");
    }

    #[test]
    fn mixed_document_with_heading_paragraph_list_and_code() {
        let md = "\
# Summary

This is a **bold** update.

- first item
- second `code` item

```rust
fn main() {}
```
";
        let v = markdown_to_adf(md);
        let blocks: Vec<&str> = v["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            blocks,
            vec!["heading", "paragraph", "bulletList", "codeBlock"]
        );
    }
}
