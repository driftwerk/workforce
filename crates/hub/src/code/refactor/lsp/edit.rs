//! Positions in the encoding a server speaks, and a server's `WorkspaceEdit`
//! as text edits per document.

use serde_json::{json, Value};

/// How a server counts the characters of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
    Utf32,
}

impl Encoding {
    /// The encoding a server chose in its `initialize` result; UTF-16 when
    /// it names none, as the protocol says.
    pub fn chosen(capabilities: &Value) -> Self {
        match capabilities.get("positionEncoding").and_then(Value::as_str) {
            Some("utf-8") => Self::Utf8,
            Some("utf-32") => Self::Utf32,
            _ => Self::Utf16,
        }
    }

    fn width(self, c: char) -> usize {
        match self {
            Self::Utf8 => c.len_utf8(),
            Self::Utf16 => c.len_utf16(),
            Self::Utf32 => 1,
        }
    }
}

/// The position of byte `offset` in `text`.
pub fn position(text: &str, offset: usize, encoding: Encoding) -> Value {
    let before = &text[..offset];
    let line_start = before.rfind('\n').map_or(0, |at| at + 1);
    let character: usize = before[line_start..]
        .chars()
        .map(|c| encoding.width(c))
        .sum();
    json!({"line": before.matches('\n').count(), "character": character})
}

/// The byte offset of `position` in `text`; a character past the end of its
/// line is the line's end, as the protocol says. `None` past the last line.
pub fn offset(text: &str, position: &Value, encoding: Encoding) -> Option<usize> {
    let line = usize::try_from(position.get("line")?.as_u64()?).ok()?;
    let character = usize::try_from(position.get("character")?.as_u64()?).ok()?;
    let line_start = match line {
        0 => 0,
        _ => text.match_indices('\n').nth(line - 1)?.0 + 1,
    };
    let rest = &text[line_start..];
    let line_text = &rest[..rest.find('\n').unwrap_or(rest.len())];
    let line_text = line_text.strip_suffix('\r').unwrap_or(line_text);
    let mut units = 0;
    for (at, c) in line_text.char_indices() {
        if units >= character {
            return Some(line_start + at);
        }
        units += encoding.width(c);
    }
    Some(line_start + line_text.len())
}

/// One replacement in one document.
#[derive(Debug, Clone, PartialEq)]
pub struct TextEdit {
    pub range: Value,
    pub new_text: String,
}

/// The edits a `WorkspaceEdit` makes to one document.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentEdits {
    pub uri: String,
    /// The document version the edits were computed against, when the
    /// server says.
    pub version: Option<i64>,
    pub edits: Vec<TextEdit>,
}

/// The documents a `WorkspaceEdit` changes. One that would create, rename or
/// delete a file is refused whole.
pub fn document_edits(edit: &Value) -> Result<Vec<DocumentEdits>, String> {
    let mut documents: Vec<DocumentEdits> = Vec::new();
    let mut add = |uri: &str, version: Option<i64>, edits: &Value| -> Result<(), String> {
        let edits = edits
            .as_array()
            .ok_or("an edit list that is not a list")?
            .iter()
            .map(|edit| {
                Ok(TextEdit {
                    range: edit
                        .get("range")
                        .cloned()
                        .ok_or("an edit without a range")?,
                    new_text: edit
                        .get("newText")
                        .and_then(Value::as_str)
                        .ok_or("an edit without its text")?
                        .to_string(),
                })
            })
            .collect::<Result<Vec<_>, &str>>()?;
        match documents.iter_mut().find(|document| document.uri == uri) {
            Some(document) => document.edits.extend(edits),
            None => documents.push(DocumentEdits {
                uri: uri.to_string(),
                version,
                edits,
            }),
        }
        Ok(())
    };
    match edit.get("documentChanges").and_then(Value::as_array) {
        Some(changes) => {
            for change in changes {
                if change.get("kind").is_some() {
                    return Err("the edit would create, rename or delete files, which code.refactor does not do".into());
                }
                let uri = change
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .ok_or("a document change without a document")?;
                let version = change
                    .pointer("/textDocument/version")
                    .and_then(Value::as_i64);
                add(uri, version, change.get("edits").unwrap_or(&Value::Null))?;
            }
        }
        None => {
            if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
                for (uri, edits) in changes {
                    add(uri, None, edits)?;
                }
            }
        }
    }
    Ok(documents)
}

/// `text` with `edits` made, each replaced span first held to `expected`.
pub fn apply_edits(
    text: &str,
    edits: &[TextEdit],
    encoding: Encoding,
    expected: impl Fn(&str) -> bool,
) -> Result<String, String> {
    let mut spans = edits
        .iter()
        .map(|edit| {
            let start = edit
                .range
                .get("start")
                .and_then(|p| offset(text, p, encoding));
            let end = edit
                .range
                .get("end")
                .and_then(|p| offset(text, p, encoding));
            match (start, end) {
                (Some(start), Some(end)) if start <= end => {
                    Ok((start, end, edit.new_text.as_str()))
                }
                _ => Err("an edit outside the document".to_string()),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    spans.sort_by_key(|(start, end, _)| (*start, *end));
    if spans.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err("edits that overlap".into());
    }
    if let Some((start, end, _)) = spans
        .iter()
        .find(|(start, end, _)| start < end && !expected(&text[*start..*end]))
    {
        return Err(format!(
            "an edit over `{}`, which is not what the server was asked to change",
            &text[*start..*end]
        ));
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end, new_text) in spans {
        out.push_str(&text[at..start]);
        out.push_str(new_text);
        at = end;
    }
    out.push_str(&text[at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_in_the_servers_encoding() {
        let text = "a\nlet é𝄞x = 1;\r\nend";
        let x = text.find('x').unwrap();
        assert_eq!(
            position(text, x, Encoding::Utf8),
            json!({"line": 1, "character": 10})
        );
        assert_eq!(
            position(text, x, Encoding::Utf16),
            json!({"line": 1, "character": 7})
        );
        assert_eq!(
            position(text, x, Encoding::Utf32),
            json!({"line": 1, "character": 6})
        );
        for encoding in [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32] {
            assert_eq!(
                offset(text, &position(text, x, encoding), encoding),
                Some(x)
            );
        }
        let past = json!({"line": 1, "character": 99});
        assert_eq!(offset(text, &past, Encoding::Utf16), text.find('\r'));
        assert_eq!(
            offset(text, &json!({"line": 9, "character": 0}), Encoding::Utf16),
            None
        );
    }

    #[test]
    fn document_changes_and_plain_changes_both_read_and_resource_operations_are_refused() {
        let range =
            json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}});
        let versioned = json!({"documentChanges": [
            {"textDocument": {"uri": "file:///a.rs", "version": 3}, "edits": [{"range": range, "newText": "b"}]},
            {"textDocument": {"uri": "file:///a.rs", "version": 3}, "edits": [{"range": range, "newText": "c"}]},
        ]});
        let documents = document_edits(&versioned).unwrap();
        assert_eq!(documents.len(), 1);
        assert_eq!(
            (documents[0].version, documents[0].edits.len()),
            (Some(3), 2)
        );

        let plain = json!({"changes": {"file:///b.ts": [{"range": range, "newText": "x"}]}});
        assert_eq!(document_edits(&plain).unwrap()[0].uri, "file:///b.ts");

        let renames_a_file = json!({"documentChanges": [
            {"kind": "rename", "oldUri": "file:///a.rs", "newUri": "file:///b.rs"},
        ]});
        assert!(document_edits(&renames_a_file).is_err());
    }

    fn edit(line: u64, from: u64, to: u64, text: &str) -> TextEdit {
        TextEdit {
            range: json!({"start": {"line": line, "character": from}, "end": {"line": line, "character": to}}),
            new_text: text.into(),
        }
    }

    #[test]
    fn edits_apply_together_and_a_span_that_is_not_the_old_name_refuses_them() {
        let text = "fn old() {}\nfn main() { old(); }\n";
        let edits = [edit(1, 12, 15, "new"), edit(0, 3, 6, "new")];
        let renamed = apply_edits(text, &edits, Encoding::Utf8, |span| span == "old").unwrap();
        assert_eq!(renamed, "fn new() {}\nfn main() { new(); }\n");

        let shifted = [edit(1, 11, 14, "new")];
        let err = apply_edits(text, &shifted, Encoding::Utf8, |span| span == "old").unwrap_err();
        assert!(err.contains("not what the server was asked"), "{err}");

        let overlapping = [edit(0, 3, 6, "a"), edit(0, 5, 7, "b")];
        assert!(apply_edits(text, &overlapping, Encoding::Utf8, |_| true).is_err());
    }
}
