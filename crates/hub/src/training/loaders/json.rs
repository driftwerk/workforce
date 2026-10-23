use eyre::{eyre, Result, WrapErr};
use serde::Deserialize;

use super::RawChunk;

/// Top-level JSON shape per DW-536:
/// `{ "name": "...", "version": 1, "entries": [{ "title", "body", "tags"?, "aliases"? }] }`.
/// The optional `name` and `version` are accepted but unused by the
/// loader — the caller supplies the corpus name.
#[derive(Debug, Deserialize)]
struct CorpusDoc {
    #[serde(default)]
    entries: Vec<EntryDoc>,
}

#[derive(Debug, Deserialize)]
struct EntryDoc {
    #[serde(default)]
    title: Option<String>,
    body: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    aliases: Vec<String>,
}

pub fn parse(bytes: &[u8]) -> Result<Vec<RawChunk>> {
    let doc: CorpusDoc = serde_json::from_slice(bytes).wrap_err("parse training JSON")?;
    if doc.entries.is_empty() {
        return Ok(Vec::new());
    }
    doc.entries
        .into_iter()
        .enumerate()
        .map(|(idx, entry)| {
            if entry.body.trim().is_empty() {
                return Err(eyre!("entry {idx}: empty body"));
            }
            let mut tags = entry.tags;
            for alias in entry.aliases {
                tags.push(format!("alias:{alias}"));
            }
            Ok(RawChunk {
                title: entry.title,
                body: entry.body,
                locator: format!("entries[{idx}]"),
                tags,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_corpus() {
        let raw = br#"{
            "name": "Lakeside",
            "version": 1,
            "entries": [
                {"title": "LCC", "body": "Lakeside Community Centre", "aliases": ["Lakeside Centre"]},
                {"body": "Free-form note"}
            ]
        }"#;
        let chunks = parse(raw).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].title.as_deref(), Some("LCC"));
        assert_eq!(chunks[0].body, "Lakeside Community Centre");
        assert!(chunks[0]
            .tags
            .contains(&"alias:Lakeside Centre".to_string()));
        assert_eq!(chunks[0].locator, "entries[0]");
        assert!(chunks[1].title.is_none());
    }

    #[test]
    fn empty_entries_yields_empty_chunks() {
        let raw = br#"{"entries": []}"#;
        assert!(parse(raw).unwrap().is_empty());
    }

    #[test]
    fn missing_entries_field_yields_empty_chunks() {
        let raw = br#"{"name": "x", "version": 1}"#;
        assert!(parse(raw).unwrap().is_empty());
    }

    #[test]
    fn rejects_entry_with_empty_body() {
        let raw = br#"{"entries": [{"title": "T", "body": "   "}]}"#;
        assert!(parse(raw).is_err());
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse(b"not json").is_err());
    }
}
