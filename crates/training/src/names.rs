//! Shared name-validation helpers used by every surface that takes a
//! corpus name (CLI direct mode, CLI remote mode, HTTP handlers, future
//! Slack ingest, future web UI).
//!
//! The two functions enforce two different shapes:
//!
//! - [`validate_slug`] is for **user-supplied identifiers** — corpus
//!   names, future skill ids. They must roundtrip through URLs, SQL,
//!   filesystem paths, and the prompt. Anything outside
//!   `[A-Za-z0-9._-]` is rejected so the caller fixes the input rather
//!   than silently operating on a different name.
//! - [`sanitize_filename`] is for **user-supplied metadata** — the
//!   original filename a user uploaded. We must keep the label as
//!   close to verbatim as possible (it shows up in citations) but
//!   collapse anything that can't safely become an on-disk segment.
//!
//! Both have explicit tests for the path-traversal vectors —
//! historically the call sites mangled these instead of rejecting them,
//! which masked operator typos.

use std::fmt;

/// Upper bound on slug length. Generous on the high side (corpus
/// names sometimes encode topic + version, e.g. `product-catalogue-v2`) but
/// low enough to keep prompts and tables tidy.
pub const MAX_SLUG_LEN: usize = 64;

/// Returned by [`validate_slug`] when the input doesn't meet the slug
/// rules. Display impl renders a user-facing reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlugError {
    Empty,
    TooLong { actual: usize, max: usize },
    InvalidChar { ch: char, position: usize },
}

impl fmt::Display for SlugError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "is empty"),
            Self::TooLong { actual, max } => {
                write!(f, "is {actual} chars (max {max})")
            }
            Self::InvalidChar { ch, position } => write!(
                f,
                "contains invalid character {ch:?} at position {position} \
                 (allowed: ASCII alphanumeric, `-`, `_`, `.`)",
            ),
        }
    }
}

impl std::error::Error for SlugError {}

/// Validate that `raw` is a safe slug for a corpus, agent id, or any
/// other identifier that needs to roundtrip through URLs and SQL.
///
/// Rules:
/// - non-empty
/// - ≤ [`MAX_SLUG_LEN`] characters
/// - only ASCII alphanumeric, `-`, `_`, `.`
///
/// Returns the slug back on success so callers can chain without
/// re-borrowing.
pub fn validate_slug(raw: &str) -> Result<&str, SlugError> {
    if raw.is_empty() {
        return Err(SlugError::Empty);
    }
    if raw.len() > MAX_SLUG_LEN {
        return Err(SlugError::TooLong {
            actual: raw.len(),
            max: MAX_SLUG_LEN,
        });
    }
    for (i, ch) in raw.chars().enumerate() {
        if !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.') {
            return Err(SlugError::InvalidChar { ch, position: i });
        }
    }
    Ok(raw)
}

/// Sanitise a user-supplied filename for safe on-disk use while
/// keeping it close to the original for the citation label.
///
/// Strips any leading directory component, replaces unsafe characters
/// with `_`, and falls back to `"upload.bin"` if the result is empty
/// or all-unsafe. Never panics; never returns an empty string.
///
/// Use this **only** for filenames; for any identifier that needs to
/// stay stable for URLs/SQL use [`validate_slug`].
pub fn sanitize_filename(raw: &str) -> String {
    // Path::file_name strips any leading directory components.
    let basename = std::path::Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(raw);

    let mut out = String::with_capacity(basename.len());
    for ch in basename.chars() {
        if ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.' | ' ' | '(' | ')' | '[' | ']') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    // Trim leading dots so we don't produce dotfile-style names like
    // `..something.txt` from `../../something.txt`.
    let trimmed: String = out.trim_start_matches('.').to_string();
    if trimmed.is_empty() {
        "upload.bin".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_slug_accepts_typical_corpus_names() {
        assert_eq!(validate_slug("product-catalogue"), Ok("product-catalogue"));
        assert_eq!(validate_slug("Q3_2026"), Ok("Q3_2026"));
        assert_eq!(validate_slug("v1.0"), Ok("v1.0"));
        assert_eq!(validate_slug("a"), Ok("a"));
    }

    #[test]
    fn validate_slug_rejects_empty() {
        assert_eq!(validate_slug(""), Err(SlugError::Empty));
    }

    #[test]
    fn validate_slug_rejects_path_separators() {
        assert!(matches!(
            validate_slug("a/b"),
            Err(SlugError::InvalidChar { ch: '/', .. })
        ));
        assert!(matches!(
            validate_slug("a\\b"),
            Err(SlugError::InvalidChar { ch: '\\', .. })
        ));
    }

    #[test]
    fn validate_slug_rejects_traversal() {
        // `..` alone is allowed (just dots); `..` followed by `/` is
        // rejected because `/` isn't in the allowed set.
        assert_eq!(validate_slug(".."), Ok(".."));
        assert!(validate_slug("../etc").is_err());
    }

    #[test]
    fn validate_slug_rejects_spaces_and_specials() {
        assert!(validate_slug("hello world").is_err());
        assert!(validate_slug("a:b").is_err());
        assert!(validate_slug("a;b").is_err());
        assert!(validate_slug("a&b").is_err());
        assert!(validate_slug("a?b").is_err());
    }

    #[test]
    fn validate_slug_rejects_too_long() {
        let s = "a".repeat(MAX_SLUG_LEN + 1);
        assert!(matches!(
            validate_slug(&s),
            Err(SlugError::TooLong { actual, max })
                if actual == MAX_SLUG_LEN + 1 && max == MAX_SLUG_LEN
        ));
    }

    #[test]
    fn validate_slug_rejects_non_ascii() {
        assert!(matches!(
            validate_slug("café"),
            Err(SlugError::InvalidChar { ch: 'é', .. })
        ));
    }

    #[test]
    fn sanitize_filename_keeps_typical_names() {
        assert_eq!(sanitize_filename("sites.json"), "sites.json");
        assert_eq!(
            sanitize_filename("My Notes (final).pdf"),
            "My Notes (final).pdf",
        );
    }

    #[test]
    fn sanitize_filename_strips_path_components() {
        // Unix path separators are stripped by `Path::file_name`.
        assert_eq!(sanitize_filename("../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("/abs/path/to/file.txt"), "file.txt");
    }

    #[test]
    fn sanitize_filename_rejects_windows_separators() {
        // On Unix `Path::file_name` doesn't treat `\` as a separator, so
        // the character-class sweep is the only thing stopping a
        // Windows-shaped traversal vector from leaking through.
        let cleaned = sanitize_filename("a\\b\\c.txt");
        assert!(
            !cleaned.contains('\\'),
            "backslash must not survive sanitisation; got {cleaned:?}",
        );
    }

    #[test]
    fn sanitize_filename_replaces_unsafe_chars() {
        assert_eq!(sanitize_filename("weird:name?.txt"), "weird_name_.txt");
    }

    #[test]
    fn sanitize_filename_falls_back_on_pathological() {
        assert_eq!(sanitize_filename(""), "upload.bin");
        assert_eq!(sanitize_filename("."), "upload.bin");
        assert_eq!(sanitize_filename(".."), "upload.bin");
        assert_eq!(sanitize_filename("..."), "upload.bin");
    }

    #[test]
    fn sanitize_filename_keeps_unicode_alphanumeric() {
        assert_eq!(sanitize_filename("café.pdf"), "café.pdf");
    }
}
