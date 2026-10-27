//! A page under edit. The curator never re-types a page: each change names
//! one exact passage of the current text and its replacement, and applies
//! only when that passage occurs exactly once. Text no edit names is never
//! re-emitted by the model, so it stays byte-for-byte as it was, and a
//! misquoted passage fails to match instead of corrupting the page.

use std::fmt;

/// Why an edit was refused. Shown to the model so it can correct the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// `old_text` was empty.
    EmptyPassage,
    /// `old_text` doesn't occur in the page as it stands.
    NotFound,
    /// `old_text` occurs this many times; it must name one place.
    Ambiguous(usize),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPassage => write!(f, "old_text is empty; quote the passage to replace"),
            Self::NotFound => write!(
                f,
                "old_text does not occur in the page as it stands; copy it exactly from the \
                 page, including whitespace and punctuation, and account for your earlier edits"
            ),
            Self::Ambiguous(n) => write!(
                f,
                "old_text occurs {n} times; quote more of the surrounding text so it names one \
                 place"
            ),
        }
    }
}

/// One applied change, kept for the pull request's account of the edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppliedEdit {
    Replaced { old_text: String, new_text: String },
    Appended { text: String },
}

/// The page as the curator is editing it.
#[derive(Debug, Clone)]
pub struct PageDraft {
    original: String,
    current: String,
    applied: Vec<AppliedEdit>,
}

impl PageDraft {
    /// A draft of `original`, or of an empty page when the page doesn't
    /// exist yet.
    pub fn new(original: Option<&str>) -> Self {
        let original = original.unwrap_or_default().to_string();
        Self {
            current: original.clone(),
            original,
            applied: Vec::new(),
        }
    }

    pub fn text(&self) -> &str {
        &self.current
    }

    pub fn applied(&self) -> &[AppliedEdit] {
        &self.applied
    }

    pub fn is_changed(&self) -> bool {
        self.current != self.original
    }

    /// Where the one occurrence of `old_text` starts, or why there isn't one.
    pub fn locate(&self, old_text: &str) -> Result<usize, EditError> {
        if old_text.is_empty() {
            return Err(EditError::EmptyPassage);
        }
        let mut found = self.current.match_indices(old_text);
        let Some((at, _)) = found.next() else {
            return Err(EditError::NotFound);
        };
        match found.count() {
            0 => Ok(at),
            extra => Err(EditError::Ambiguous(extra + 1)),
        }
    }

    /// Replace the one occurrence of `old_text` with `new_text`.
    pub fn replace(&mut self, old_text: &str, new_text: &str) -> Result<(), EditError> {
        let at = self.locate(old_text)?;
        self.current
            .replace_range(at..at + old_text.len(), new_text);
        self.applied.push(AppliedEdit::Replaced {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        });
        Ok(())
    }

    /// The section of the page that the byte offset `at` falls in: from the
    /// heading above it to the next heading, headings inside code fences
    /// not counting.
    pub fn section_around(&self, at: usize) -> &str {
        section_around(&self.current, at)
    }

    /// Add `text` to the end of the page, on a line of its own.
    pub fn append(&mut self, text: &str) {
        if !self.current.is_empty() && !self.current.ends_with('\n') {
            self.current.push('\n');
        }
        self.current.push_str(text);
        self.applied.push(AppliedEdit::Appended {
            text: text.to_string(),
        });
    }
}

/// The section of `text` that the byte offset `at` falls in: from the
/// heading line above it (or the start) to the next heading (or the end).
/// Lines inside ``` or ~~~ fences are never headings.
pub fn section_around(text: &str, at: usize) -> &str {
    let mut start = 0;
    let mut fence: Option<char> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().filter(|c| *c == '`' || *c == '~');
        if let Some(c) = marker.filter(|c| trimmed.starts_with(&c.to_string().repeat(3))) {
            fence = match fence {
                Some(open) if open == c => None,
                None => Some(c),
                other => other,
            };
        } else if fence.is_none() && trimmed.starts_with('#') {
            if offset > at {
                return &text[start..offset];
            }
            start = offset;
        }
        offset += line.len();
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "---\ntitle: CLI\n---\n\n## Flags\n\n- `--all` — every page.\n- `--no-pr` — no PR.\n\n## Shortcuts\n\n- `Ctrl+Y` — toggle mouse capture.\n";

    #[test]
    fn an_edit_applies_only_when_its_passage_occurs_exactly_once() {
        let mut draft = PageDraft::new(Some(PAGE));
        assert_eq!(draft.replace("--nope", "x"), Err(EditError::NotFound));
        assert_eq!(draft.replace("- `", "x"), Err(EditError::Ambiguous(3)));
        assert_eq!(draft.replace("", "x"), Err(EditError::EmptyPassage));
        assert_eq!(draft.text(), PAGE, "a refused edit must change nothing");
        assert!(draft.applied().is_empty());

        draft
            .replace(
                "- `--no-pr` — no PR.\n",
                "- `--no-pr` — no PR.\n- `--dry-run` — print the plan.\n",
            )
            .unwrap();
        assert!(draft.text().contains("--dry-run"));
        assert_eq!(draft.applied().len(), 1);
    }

    #[test]
    fn untouched_text_is_byte_identical_after_edits() {
        let mut draft = PageDraft::new(Some(PAGE));
        draft
            .replace("toggle mouse capture", "toggle mouse capture (TUI only)")
            .unwrap();
        let edited = draft.text();
        let (before, after) = PAGE.split_once("toggle mouse capture").unwrap();
        assert!(edited.starts_with(before));
        assert!(edited.ends_with(after));
        assert_eq!(edited.len(), PAGE.len() + " (TUI only)".len());
    }

    #[test]
    fn later_edits_see_earlier_ones() {
        let mut draft = PageDraft::new(Some(PAGE));
        draft.replace("`--all`", "`--every`").unwrap();
        assert_eq!(draft.replace("`--all`", "x"), Err(EditError::NotFound));
        draft.replace("`--every`", "`--all`").unwrap();
        assert!(
            !draft.is_changed(),
            "two edits that cancel leave the page as it was"
        );
    }

    #[test]
    fn the_section_around_an_edit_runs_heading_to_heading() {
        let page = "# A\nintro\n## B\nbody\n```sh\n# not a heading\n```\nmore\n## C\nend\n";
        let at = page.find("more").unwrap();
        assert_eq!(
            section_around(page, at),
            "## B\nbody\n```sh\n# not a heading\n```\nmore\n"
        );
        assert_eq!(
            section_around(page, page.find("end").unwrap()),
            "## C\nend\n"
        );
    }

    #[test]
    fn appending_builds_a_new_page() {
        let mut draft = PageDraft::new(None);
        draft.append("---\ntitle: New\n---\n");
        draft.append("## Usage\n\nRun it.\n");
        assert_eq!(draft.text(), "---\ntitle: New\n---\n## Usage\n\nRun it.\n");
        assert!(draft.is_changed());
    }

    #[test]
    fn appending_starts_a_new_line_when_the_page_lacks_one() {
        let mut draft = PageDraft::new(Some("# A"));
        draft.append("## B\n");
        assert_eq!(draft.text(), "# A\n## B\n");
    }
}
