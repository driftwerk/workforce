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

    /// Replace the one occurrence of `old_text` with `new_text`.
    pub fn replace(&mut self, old_text: &str, new_text: &str) -> Result<(), EditError> {
        if old_text.is_empty() {
            return Err(EditError::EmptyPassage);
        }
        let mut found = self.current.match_indices(old_text);
        let Some((at, _)) = found.next() else {
            return Err(EditError::NotFound);
        };
        let extra = found.count();
        if extra > 0 {
            return Err(EditError::Ambiguous(extra + 1));
        }
        self.current
            .replace_range(at..at + old_text.len(), new_text);
        self.applied.push(AppliedEdit::Replaced {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        });
        Ok(())
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
