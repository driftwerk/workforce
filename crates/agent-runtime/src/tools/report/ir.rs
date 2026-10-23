//! The engine-neutral report IR the agent emits. Flat and enumerated: the model
//! produces content as data, never layout, so it cannot break the rendering.

use serde::Deserialize;
use serde_json::Value;
use workforce_common::emoji;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    Report,
    Infographic,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Report {
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub format: Format,
    #[serde(default)]
    pub theme: Option<String>,
    /// Whether emoji in the content render (default) or are stripped for a
    /// strictly formal look. `None` defers to the workspace `[report].emoji`
    /// default, then to `true`. Resolved in `render_report`.
    #[serde(default)]
    pub emoji: Option<bool>,
    #[serde(default)]
    pub meta: Meta,
    #[serde(default)]
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Meta {
    #[serde(default)]
    pub kicker: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Section {
    #[serde(default)]
    pub heading: Option<String>,
    #[serde(default)]
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Prose {
        text: String,
    },
    Heading {
        text: String,
    },
    /// Preformatted text kept verbatim — the one block whose line breaks and
    /// indentation are content, so a source document's fenced code survives.
    Code {
        text: String,
        #[serde(default)]
        language: Option<String>,
    },
    Stats {
        #[serde(default)]
        items: Vec<StatItem>,
    },
    Chart {
        intent: ChartIntent,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        caption: Option<String>,
    },
    /// A node-and-arrow diagram. `spec` is the `wf-diagram` wire contract,
    /// carried verbatim so the same JSON draws the same picture here and in the
    /// customer-facing widget.
    Diagram {
        spec: Value,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        caption: Option<String>,
    },
    Table {
        #[serde(default)]
        columns: Vec<String>,
        #[serde(default)]
        rows: Vec<Vec<Value>>,
        #[serde(default)]
        caption: Option<String>,
    },
    List {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        items: Vec<ListItem>,
    },
    Image {
        artifact_id: String,
        #[serde(default)]
        caption: Option<String>,
        #[serde(default)]
        width: Option<u8>,
        #[serde(default)]
        align: Align,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StatItem {
    pub value: String,
    pub label: String,
    #[serde(default)]
    pub accent: AccentTone,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListItem {
    #[serde(default)]
    pub lead: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccentTone {
    #[default]
    Accent,
    Primary,
    Secondary,
    Ink,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChartIntent {
    pub kind: ChartKind,
    #[serde(default)]
    pub data: Vec<Value>,
    #[serde(default)]
    pub x: Option<String>,
    #[serde(default)]
    pub y: Option<String>,
    #[serde(default)]
    pub orientation: Orientation,
    #[serde(default)]
    pub value_suffix: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    #[default]
    Bar,
    Line,
    Area,
    Donut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    Vertical,
    #[default]
    Horizontal,
}

impl Report {
    /// Reject a report that has nothing to render, so the tool fails loudly
    /// rather than producing an empty document.
    ///
    /// The two content rules catch opposite shapes and neither subsumes the
    /// other: no sections at all, and a section with no blocks. The latter used
    /// to require *every* section to be empty, so one populated section out of
    /// thirteen rendered as a title page plus a list of bare headings — a silent
    /// success, which is the one failure a model cannot self-correct from.
    pub fn validate(&self) -> Result<(), String> {
        if self.title.trim().is_empty() {
            return Err("report title is empty".into());
        }
        if self.sections.is_empty() {
            return Err("report has no sections".into());
        }
        let empty: Vec<String> = self
            .sections
            .iter()
            .enumerate()
            .filter(|(_, s)| s.blocks.is_empty())
            .map(|(i, s)| match s.heading.as_deref() {
                Some(h) => h.to_string(),
                None => format!("section {}", i + 1),
            })
            .collect();
        if !empty.is_empty() {
            return Err(format!(
                "these sections have no content blocks: {empty:?} — every section needs at least \
                 one block (prose, heading, stats, chart, diagram, table, list, code, image). Fill \
                 them in, \
                 or leave them out of the report entirely."
            ));
        }
        Ok(())
    }

    /// Apply `f` to every human-text field in the report — title, subtitle,
    /// meta, section headings, and every block's prose/labels/captions/string
    /// cells. The single place that knows the report's text shape, so per-field
    /// transforms ([`normalize_text`](Self::normalize_text),
    /// [`strip_emoji`](Self::strip_emoji)) stay in sync.
    fn map_text(&mut self, f: &impl Fn(&str) -> String) {
        apply(&mut self.title, f);
        apply_opt(&mut self.subtitle, f);
        apply_opt(&mut self.meta.kicker, f);
        apply_opt(&mut self.meta.author, f);
        apply_opt(&mut self.meta.date, f);
        for sec in &mut self.sections {
            apply_opt(&mut sec.heading, f);
            for block in &mut sec.blocks {
                block.map_text(f);
            }
        }
    }

    /// Normalise text on every render: convert known Slack `:shortcode:` emoji to
    /// Unicode (the bundled Noto Emoji renders them) and drop U+FFFD replacement
    /// characters, so upstream-mangled emoji show as a clean gap instead of
    /// `���` blotches.
    pub fn normalize_text(&mut self) {
        self.map_text(&|s| emoji::shortcodes_to_unicode(s).replace('\u{FFFD}', ""));
    }

    /// Remove decorative emoji from every text field. Applied when a report
    /// resolves to `emoji: false`, so the document is strictly free of
    /// pictographs rather than rendering them via the bundled emoji fallback.
    pub fn strip_emoji(&mut self) {
        self.map_text(&|s| emoji::strip_emoji(s));
    }
}

impl Block {
    fn map_text(&mut self, f: &impl Fn(&str) -> String) {
        match self {
            Block::Prose { text } | Block::Heading { text } => apply(text, f),
            // Code is verbatim: normalising or stripping inside it would rewrite
            // the very characters the block exists to preserve.
            Block::Code { .. } => {}
            Block::Stats { items } => {
                for it in items {
                    apply(&mut it.value, f);
                    apply(&mut it.label, f);
                }
            }
            Block::Chart {
                intent,
                title,
                caption,
            } => {
                apply_opt(title, f);
                apply_opt(caption, f);
                apply_opt(&mut intent.value_suffix, f);
                for row in &mut intent.data {
                    if let Value::Object(map) = row {
                        for v in map.values_mut() {
                            if let Value::String(s) = v {
                                apply(s, f);
                            }
                        }
                    }
                }
            }
            Block::Table {
                columns,
                rows,
                caption,
            } => {
                for c in columns {
                    apply(c, f);
                }
                for cell in rows.iter_mut().flatten() {
                    if let Value::String(s) = cell {
                        apply(s, f);
                    }
                }
                apply_opt(caption, f);
            }
            Block::List { title, items } => {
                apply_opt(title, f);
                for it in items {
                    apply_opt(&mut it.lead, f);
                    apply(&mut it.text, f);
                }
            }
            Block::Diagram {
                spec,
                title,
                caption,
            } => {
                apply_opt(title, f);
                apply_opt(caption, f);
                for key in ["nodes", "participants", "edges", "steps"] {
                    let Some(items) = spec.get_mut(key).and_then(Value::as_array_mut) else {
                        continue;
                    };
                    for item in items {
                        for field in ["label", "group", "note"] {
                            if let Some(Value::String(s)) = item.get_mut(field) {
                                apply(s, f);
                            }
                        }
                    }
                }
            }
            Block::Image { caption, .. } => apply_opt(caption, f),
        }
    }
}

fn apply(s: &mut String, f: &impl Fn(&str) -> String) {
    *s = f(s);
}

fn apply_opt(s: &mut Option<String>, f: &impl Fn(&str) -> String) {
    if let Some(v) = s {
        apply(v, f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> Report {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn parses_a_full_report_with_every_block() {
        let report = parse(json!({
            "title": "Q",
            "format": "infographic",
            "theme": "slate",
            "meta": { "kicker": "K", "author": "A", "date": "D" },
            "sections": [{
                "heading": "S",
                "blocks": [
                    { "type": "prose", "text": "p" },
                    { "type": "heading", "text": "h" },
                    { "type": "stats", "items": [{ "value": "1", "label": "l", "accent": "secondary" }] },
                    { "type": "chart", "title": "t", "caption": "c",
                      "intent": { "kind": "bar", "x": "k", "y": "v", "data": [{ "k": "a", "v": 1 }] } },
                    { "type": "table", "columns": ["a"], "rows": [[1]], "caption": "c" },
                    { "type": "list", "title": "t", "items": [{ "lead": "L", "text": "x" }] },
                    { "type": "image", "artifact_id": "art_x", "width": 50, "align": "left", "caption": "c" }
                ]
            }]
        }));
        assert_eq!(report.format, Format::Infographic);
        assert_eq!(report.theme.as_deref(), Some("slate"));
        assert_eq!(report.sections[0].blocks.len(), 7);
        let Block::Image { align, width, .. } = &report.sections[0].blocks[6] else {
            panic!("expected image block");
        };
        assert_eq!(*align, Align::Left);
        assert_eq!(*width, Some(50));
    }

    #[test]
    fn defaults_format_to_report_and_accent_to_accent() {
        let report = parse(json!({
            "title": "T",
            "sections": [{ "blocks": [{ "type": "stats", "items": [{ "value": "1", "label": "l" }] }] }]
        }));
        assert_eq!(report.format, Format::Report);
        let Block::Stats { items } = &report.sections[0].blocks[0] else {
            panic!("expected stats");
        };
        assert_eq!(items[0].accent, AccentTone::Accent);
    }

    #[test]
    fn unknown_block_type_is_rejected() {
        let err = serde_json::from_value::<Report>(json!({
            "title": "T",
            "sections": [{ "blocks": [{ "type": "nope" }] }]
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown variant"), "got: {err}");
    }

    #[test]
    fn validate_rejects_empty_title_and_empty_content() {
        assert!(parse(
            json!({ "title": "T", "sections": [{ "blocks": [{ "type": "prose", "text": "x" }] }] })
        )
        .validate()
        .is_ok());
        assert!(parse(
            json!({ "title": "  ", "sections": [{ "blocks": [{ "type": "prose", "text": "x" }] }] })
        )
        .validate()
        .is_err());
        assert!(
            parse(json!({ "title": "T", "sections": [{ "heading": "h" }] }))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn validate_rejects_a_section_less_report() {
        // The `all`-over-`any` trap: `[].iter().any(…)` is false, so an empty
        // `sections` array must be caught by its own rule or it renders a bare cover.
        for spec in [
            json!({ "title": "T", "sections": [] }),
            json!({ "title": "T" }),
        ] {
            let err = parse(spec.clone()).validate().unwrap_err();
            assert!(err.contains("no sections"), "{spec}: {err}");
        }
    }

    #[test]
    fn validate_rejects_any_empty_section_and_names_it() {
        // The delivered index-page report: one populated section, twelve empty.
        let sec = |h: &str| json!({ "heading": h, "blocks": [] });
        let err = parse(json!({
            "title": "Contoso",
            "sections": [
                { "heading": "Executive summary",
                  "blocks": [{ "type": "prose", "text": "real content" }] },
                sec("Platform layer"),
                sec("Domain architecture"),
            ]
        }))
        .validate()
        .unwrap_err();
        assert!(
            err.contains("Platform layer") && err.contains("Domain architecture"),
            "names every empty section: {err}"
        );
        assert!(
            !err.contains("Executive summary"),
            "does not name the populated one: {err}"
        );
    }

    #[test]
    fn validate_labels_an_unheaded_empty_section_by_position() {
        let err = parse(json!({
            "title": "T",
            "sections": [
                { "blocks": [{ "type": "prose", "text": "x" }] },
                { "blocks": [] },
            ]
        }))
        .validate()
        .unwrap_err();
        assert!(err.contains("section 2"), "got: {err}");
    }

    #[test]
    fn code_blocks_parse_and_keep_their_text_verbatim() {
        let mut r = parse(json!({
            "title": "T",
            "sections": [{ "blocks": [
                { "type": "code", "text": "fn main() {\n\tok(); // :rocket: 100%\n}", "language": "rust" },
                { "type": "code", "text": "plain" }
            ] }]
        }));
        assert!(r.validate().is_ok());
        let Block::Code { text, language } = &r.sections[0].blocks[0] else {
            panic!("expected a code block");
        };
        assert_eq!(language.as_deref(), Some("rust"));
        assert!(text.contains('\n') && text.contains('\t'));
        assert!(
            matches!(&r.sections[0].blocks[1], Block::Code { language, .. } if language.is_none()),
            "language is optional"
        );

        // Verbatim means verbatim: neither transform may rewrite code content.
        let before = r.sections[0].blocks[0].clone();
        r.normalize_text();
        r.strip_emoji();
        let Block::Code { text: after, .. } = &r.sections[0].blocks[0] else {
            panic!("expected a code block");
        };
        let Block::Code { text: original, .. } = &before else {
            panic!("expected a code block");
        };
        assert_eq!(
            after, original,
            "shortcodes and emoji left untouched in code"
        );
    }

    #[test]
    fn emoji_defaults_to_none_and_parses_when_set() {
        let r = parse(json!({
            "title": "T", "sections": [{ "blocks": [{ "type": "prose", "text": "x" }] }]
        }));
        assert_eq!(r.emoji, None, "unset => defer to workspace default");
        let off = parse(json!({
            "title": "T", "emoji": false,
            "sections": [{ "blocks": [{ "type": "prose", "text": "x" }] }]
        }));
        assert_eq!(off.emoji, Some(false));
    }

    #[test]
    fn report_strip_emoji_walks_every_text_field() {
        let mut r = parse(json!({
            "title": "📊 Release Notes",
            "subtitle": "🚀 Shipped",
            "meta": { "kicker": "⚙ Eng", "author": "👤 Me", "date": "📅 2026" },
            "sections": [{
                "heading": "🔌 ESR",
                "blocks": [
                    { "type": "prose", "text": "Done ✅ today" },
                    { "type": "heading", "text": "🗺 Floor Plan" },
                    { "type": "stats", "items": [{ "value": "16 🎉", "label": "PRs 📦" }] },
                    { "type": "table", "columns": ["Item ⭐"], "rows": [["row 🔧 one"]], "caption": "cap ⚠" },
                    { "type": "list", "title": "List 📋", "items": [{ "lead": "Lead 🔑", "text": "text 💡" }] },
                    { "type": "chart", "title": "Chart 📈", "caption": "c 🔥",
                      "intent": { "kind": "bar", "x": "k", "y": "v", "value_suffix": "% 🔥",
                                  "data": [{ "k": "North 🌍", "v": 1 }] } },
                    { "type": "image", "artifact_id": "art_x", "caption": "Fig 1 📷" }
                ]
            }]
        }));
        r.strip_emoji();
        assert_eq!(r.title, "Release Notes");
        assert_eq!(r.subtitle.as_deref(), Some("Shipped"));
        assert_eq!(r.meta.kicker.as_deref(), Some("Eng"));
        assert_eq!(r.sections[0].heading.as_deref(), Some("ESR"));
        let blocks = &r.sections[0].blocks;
        assert!(matches!(&blocks[0], Block::Prose { text } if text == "Done today"));
        assert!(matches!(&blocks[1], Block::Heading { text } if text == "Floor Plan"));
        let Block::Stats { items } = &blocks[2] else {
            panic!()
        };
        assert_eq!(
            (items[0].value.as_str(), items[0].label.as_str()),
            ("16", "PRs")
        );
        let Block::Table {
            columns,
            rows,
            caption,
        } = &blocks[3]
        else {
            panic!()
        };
        assert_eq!(columns[0], "Item");
        assert_eq!(rows[0][0], json!("row one"));
        assert_eq!(caption.as_deref(), Some("cap"));
        let Block::List { title, items } = &blocks[4] else {
            panic!()
        };
        assert_eq!(title.as_deref(), Some("List"));
        assert_eq!(
            (items[0].lead.as_deref(), items[0].text.as_str()),
            (Some("Lead"), "text")
        );
        // Charts strip their suffix and category labels so emoji never leak into the SVG.
        let Block::Chart {
            intent,
            title,
            caption,
        } = &blocks[5]
        else {
            panic!()
        };
        assert_eq!(
            (title.as_deref(), caption.as_deref()),
            (Some("Chart"), Some("c"))
        );
        assert_eq!(intent.value_suffix.as_deref(), Some("%"));
        assert_eq!(intent.data[0]["k"], json!("North"));
        let Block::Image { caption, .. } = &blocks[6] else {
            panic!()
        };
        assert_eq!(caption.as_deref(), Some("Fig 1"));
    }

    #[test]
    fn normalize_text_converts_shortcodes_and_drops_replacement_chars() {
        let mut r = parse(json!({
            "title": "Status :white_check_mark:",
            "sections": [{ "blocks": [
                { "type": "prose", "text": "M-2 handler \u{FFFD}\u{FFFD}\u{FFFD} Resolved :mag:" },
                { "type": "list", "items": [{ "lead": ":rocket: Ship", "text": "done \u{FFFD}" }] }
            ] }]
        }));
        r.normalize_text();
        assert_eq!(r.title, "Status ✅");
        let blocks = &r.sections[0].blocks;
        assert!(
            matches!(&blocks[0], Block::Prose { text } if text == "M-2 handler  Resolved 🔍"),
            "shortcode converted and U+FFFD stripped"
        );
        let Block::List { items, .. } = &blocks[1] else {
            panic!()
        };
        assert_eq!(items[0].lead.as_deref(), Some("🚀 Ship"));
        assert_eq!(items[0].text, "done ");
    }
}
