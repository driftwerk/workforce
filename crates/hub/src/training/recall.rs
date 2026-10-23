//! Renders the `## Agent Training` prompt block from recalled
//! training-tagged memories, with per-chunk citation markers resolved
//! through the document map provided by the caller.

use std::collections::HashMap;

use workforce_memory::types::Memory;

use super::DOC_TAG_PREFIX;

/// Format a list of training memories into the agent prompt block.
/// `documents` maps the doc-short-id (8 hex chars, embedded in each
/// memory's `doc:<id>` tag) to the human-readable source label —
/// typically the original filename. Memories whose doc tag has no
/// entry in `documents` render without a citation.
const INSTRUCTION_HEADER: &str =
    "Each entry below is a verbatim excerpt from a training document. When you quote or \
     paraphrase one, cite it inline with `[[cite:<short_id>]]` (e.g. \"the policy says X \
     [[cite:abc12345]]\") — the marker renders as a clickable download link in the user's \
     view. If the user asks where something came from or which documents you cited, call \
     the `training_citations` tool to surface the previous turn's sources with scores.";

pub fn format_training_section(memories: &[Memory], documents: &HashMap<String, String>) -> String {
    if memories.is_empty() {
        return String::new();
    }
    let body = memories
        .iter()
        .map(|m| format_one(m, documents))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{INSTRUCTION_HEADER}\n\n{body}")
}

fn format_one(memory: &Memory, documents: &HashMap<String, String>) -> String {
    let body = compact_body(&memory.content);
    let short_id = doc_short_id(memory);
    let filename = short_id.and_then(|s| documents.get(s).map(String::as_str));
    match (short_id, filename) {
        (Some(short), Some(name)) => format!("- [{name} · doc:{short}] {body}"),
        (Some(short), None) => format!("- [doc:{short}] {body}"),
        _ => format!("- {body}"),
    }
}

fn doc_short_id(memory: &Memory) -> Option<&str> {
    memory
        .tags
        .iter()
        .find_map(|t| t.strip_prefix(DOC_TAG_PREFIX))
}

/// Strip excessive whitespace from a chunk body. Multi-line chunks
/// collapse to a single space-separated string so the prompt block
/// stays scannable.
fn compact_body(content: &str) -> String {
    content.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_core::types::AgentId;
    use workforce_memory::types::{MemoryLayer, Visibility};

    fn mem(content: &str, doc_tag: Option<&str>) -> Memory {
        let mut m = Memory::new(
            content.to_string(),
            MemoryLayer::Organisation,
            AgentId("a".into()),
        );
        m.visibility = Visibility::Global;
        if let Some(d) = doc_tag {
            m.tags.push(format!("{DOC_TAG_PREFIX}{d}"));
        }
        m
    }

    fn body_of(rendered: &str) -> String {
        rendered
            .lines()
            .skip_while(|l| !l.starts_with('-'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn output_starts_with_citation_instruction() {
        let m = mem("Lakeside Community Centre", Some("abcd1234"));
        let docs = HashMap::from([("abcd1234".to_string(), "sites.json".to_string())]);
        let out = format_training_section(&[m], &docs);
        assert!(
            out.contains("[[cite:"),
            "instruction header should mention the citation marker: {out}",
        );
        assert!(
            out.contains("training_citations"),
            "instruction header should mention the citations tool: {out}",
        );
    }

    #[test]
    fn renders_chunk_with_filename_and_short_id() {
        let m = mem("Lakeside Community Centre", Some("abcd1234"));
        let docs = HashMap::from([("abcd1234".to_string(), "sites.json".to_string())]);
        let out = format_training_section(&[m], &docs);
        assert_eq!(
            body_of(&out),
            "- [sites.json · doc:abcd1234] Lakeside Community Centre",
        );
    }

    #[test]
    fn renders_chunk_without_doc_tag_unattributed() {
        let m = mem("Some fact", None);
        let docs = HashMap::new();
        let out = format_training_section(&[m], &docs);
        assert_eq!(body_of(&out), "- Some fact");
    }

    #[test]
    fn renders_chunk_with_unknown_doc_tag_keeps_short_id() {
        let m = mem("Some fact", Some("notindocs"));
        let docs = HashMap::new();
        let out = format_training_section(&[m], &docs);
        assert_eq!(body_of(&out), "- [doc:notindocs] Some fact");
    }

    #[test]
    fn collapses_whitespace() {
        let m = mem("Title\n\nBody\n  with   ragged   space", Some("d1"));
        let docs = HashMap::from([("d1".to_string(), "x.json".to_string())]);
        let out = format_training_section(&[m], &docs);
        assert_eq!(
            body_of(&out),
            "- [x.json · doc:d1] Title Body with ragged space",
        );
    }

    #[test]
    fn empty_input_produces_empty_string() {
        assert_eq!(format_training_section(&[], &HashMap::new()), "");
    }

    #[test]
    fn renders_multiple_chunks_one_per_line() {
        let ms = vec![mem("First", Some("d1")), mem("Second", Some("d2"))];
        let docs = HashMap::from([
            ("d1".to_string(), "a.json".to_string()),
            ("d2".to_string(), "b.json".to_string()),
        ]);
        let out = format_training_section(&ms, &docs);
        assert_eq!(
            body_of(&out),
            "- [a.json · doc:d1] First\n- [b.json · doc:d2] Second",
        );
    }
}
