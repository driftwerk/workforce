//! `help` — the end-user docs served to a remote MCP client as markdown, so
//! the model reads how to author a workflow, playbook or skill, grant a
//! skill, or answer an approval from the same pages people read. The pages
//! are embedded at build time, so every install ships the docs that match
//! its binary.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde::Deserialize;

use workforce_agent::tools::{Tool, ToolResult};

#[derive(rust_embed::Embed)]
#[folder = "../../end-user-docs/"]
#[include = "**/*.mdx"]
struct Docs;

/// The page that explains each tool family on the remote surface. Adding a
/// family without a page fails the test below, so the docs cannot lag the
/// tools.
const FAMILY_TOPICS: &[(&str, &str)] = &[
    ("workflow", "concepts/workflows"),
    ("playbook", "concepts/playbooks"),
    ("scheduler", "concepts/schedules"),
    ("reminder", "concepts/schedules"),
    ("wasm_skill", "concepts/skills"),
    ("skills", "concepts/skills"),
    ("skill", "concepts/skills"),
    ("system", "concepts/skills"),
    ("vault", "configuration/vault"),
    ("secret", "configuration/vault"),
    ("identity", "reference/remote-mcp"),
    ("agent", "reference/remote-mcp"),
    ("approval", "reference/remote-mcp"),
    ("help", "reference/remote-mcp"),
    ("pipeline", "concepts/pipelines"),
    ("hubspot", "adapters/hubspot"),
];

#[derive(Debug)]
pub struct Page {
    pub topic: String,
    pub title: String,
    pub description: String,
    pub markdown: String,
}

/// A page's frontmatter field, or empty when the page declares none.
fn frontmatter_field(header: &str, key: &str) -> String {
    header
        .lines()
        .find_map(|line| {
            line.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(':'))
        })
        .map(|value| value.trim().trim_matches('"').to_string())
        .unwrap_or_default()
}

fn page(path: &str, text: &str) -> Page {
    let topic = path.trim_end_matches(".mdx").to_string();
    let (header, body) = text
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .unwrap_or(("", text));
    Page {
        topic,
        title: frontmatter_field(header, "title"),
        description: frontmatter_field(header, "description"),
        markdown: body.trim().to_string(),
    }
}

/// Every embedded page, in topic order, parsed once.
pub fn pages() -> &'static [Page] {
    static PAGES: OnceLock<Vec<Page>> = OnceLock::new();
    PAGES.get_or_init(|| {
        let mut pages: Vec<Page> = Docs::iter()
            .filter_map(|path| {
                let file = Docs::get(&path)?;
                let text = std::str::from_utf8(&file.data).ok()?;
                Some(page(&path, text))
            })
            .collect();
        pages.sort_by(|a, b| a.topic.cmp(&b.topic));
        pages
    })
}

/// The page for `topic`: a tool family it explains, its full path, or the
/// last path segment when that names exactly one page.
pub fn resolve(topic: &str) -> Result<&'static Page, String> {
    let wanted = topic.trim().trim_end_matches(".mdx").trim_matches('/');
    let wanted = FAMILY_TOPICS
        .iter()
        .find(|(family, _)| *family == wanted)
        .map_or(wanted, |(_, page)| page);
    if let Some(page) = pages().iter().find(|p| p.topic == wanted) {
        return Ok(page);
    }
    let by_name: Vec<&Page> = pages()
        .iter()
        .filter(|p| p.topic.rsplit('/').next() == Some(wanted))
        .collect();
    match by_name.as_slice() {
        [page] => Ok(page),
        [] => Err(format!(
            "no help topic {topic:?}; call help with no topic to list them"
        )),
        several => Err(format!(
            "{topic:?} names several pages: {}",
            several
                .iter()
                .map(|p| p.topic.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

#[derive(Deserialize)]
struct HelpArgs {
    topic: Option<String>,
}

struct HelpTool;

#[async_trait]
impl Tool for HelpTool {
    fn name(&self) -> &str {
        "help"
    }

    fn description(&self) -> &str {
        "Read this install's documentation. Call it with no topic to list the pages, or with a topic — a page path (concepts/workflows), a tool family (workflow, playbook, wasm_skill, skill, pipeline, hubspot, vault, scheduler, approval) or a page name (skills) — to get that page as markdown. Read the relevant page before authoring a workflow, playbook or skill, granting a skill, or handling an approval."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "topic": { "type": "string", "description": "A page path, tool family or page name; omit to list the pages" }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let args: HelpArgs = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return Ok(ToolResult::failure(format!("invalid arguments: {e}"), 0)),
        };
        Ok(
            match args.topic.as_deref().filter(|t| !t.trim().is_empty()) {
                None => ToolResult::success(
                    serde_json::json!({
                        "topics": pages().iter().map(|p| serde_json::json!({
                            "topic": p.topic,
                            "title": p.title,
                            "description": p.description,
                        })).collect::<Vec<_>>(),
                        "next": "call help again with one of these topics",
                    }),
                    0,
                ),
                Some(topic) => match resolve(topic) {
                    Ok(page) => ToolResult::success(
                        serde_json::json!({
                            "topic": page.topic,
                            "title": page.title,
                            "markdown": page.markdown,
                        }),
                        0,
                    ),
                    Err(reason) => ToolResult::failure(reason, 0),
                },
            },
        )
    }
}

pub fn help_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(HelpTool)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failed_tools::tool_family;
    use workforce_common::settings::RemoteMcpConfig;

    #[test]
    fn every_page_has_a_title_and_a_body() {
        let pages = pages();
        assert!(pages.iter().any(|p| p.topic == "concepts/workflows"));
        for page in pages {
            assert!(!page.title.is_empty(), "{} has no title", page.topic);
            assert!(!page.markdown.is_empty(), "{} has no body", page.topic);
            assert!(
                !page.markdown.starts_with("---"),
                "{} keeps its frontmatter",
                page.topic
            );
        }
    }

    #[test]
    fn every_remote_tool_family_has_a_page() {
        for pattern in RemoteMcpConfig::DEFAULT_TOOLS_ALLOW {
            let family = tool_family(pattern.trim_end_matches('*').trim_end_matches('.'));
            let topic = FAMILY_TOPICS
                .iter()
                .find(|(f, _)| *f == family)
                .map(|(_, topic)| *topic)
                .unwrap_or_else(|| panic!("tool family {family} has no help topic"));
            assert!(
                pages().iter().any(|p| p.topic == topic),
                "help topic {topic} for {family} is not a docs page"
            );
        }
    }

    #[test]
    fn topics_resolve_by_path_family_and_name() {
        assert_eq!(
            resolve("concepts/workflows").unwrap().topic,
            "concepts/workflows"
        );
        assert_eq!(resolve("workflow").unwrap().topic, "concepts/workflows");
        assert_eq!(resolve("workflows").unwrap().topic, "concepts/workflows");
        for family in ["wasm_skill", "system", "skill"] {
            assert_eq!(
                resolve(family).unwrap().topic,
                "concepts/skills",
                "{family}"
            );
        }
        assert_eq!(resolve("approval").unwrap().topic, "reference/remote-mcp");
        assert_eq!(resolve("vault.mdx").unwrap().topic, "configuration/vault");
        assert_eq!(
            resolve("index").unwrap().topic,
            "index",
            "an exact topic wins over pages that merely share its name"
        );
        assert!(resolve("nope").unwrap_err().contains("no help topic"));
    }

    #[test]
    fn frontmatter_is_read_and_stripped() {
        let parsed = page(
            "a/b.mdx",
            "---\ntitle: \"B page\"\ndescription: What b is.\n---\n\n# B\n\nBody.",
        );
        assert_eq!(parsed.topic, "a/b");
        assert_eq!(parsed.title, "B page");
        assert_eq!(parsed.description, "What b is.");
        assert_eq!(parsed.markdown, "# B\n\nBody.");
        let bare = page("c.mdx", "# C");
        assert_eq!(bare.title, "");
        assert_eq!(bare.markdown, "# C");
    }
}
