//! Type-driven guard against the OSS `text2vec-transformers` empty-input crash.
//!
//! `semitechnologies/transformers-inference` panics with
//! `AttributeError: 'int' object has no attribute 'detach'` when handed a
//! string that produces zero tokens — empty, whitespace-only, or
//! punctuation/emoji-only inputs all trigger it. Constructing a
//! [`VectorizableText`] is the only way to satisfy the Weaviate client's
//! signature, so non-vectorizable strings can't reach the wire.

use std::fmt;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum VectorizableTextError {
    Empty,
    NoAlphanumeric,
}

impl VectorizableTextError {
    pub fn as_metric_label(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::NoAlphanumeric => "no_alphanumeric",
        }
    }
}

impl fmt::Display for VectorizableTextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "vectorizable text rejected: empty after trim",
            Self::NoAlphanumeric => "vectorizable text rejected: no alphanumeric characters",
        })
    }
}

impl std::error::Error for VectorizableTextError {}

/// A string guaranteed to produce at least one usable token through the
/// `text2vec-transformers` pipeline: it contains at least one Unicode
/// alphanumeric character after trimming.
///
/// Only constructible via [`try_new`]; the invariant therefore holds for
/// every instance. Serialises as its inner string so it embeds directly
/// in JSON payloads. Deliberately does not implement `Deserialize` —
/// round-tripping arbitrary input would defeat the invariant.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
#[serde(transparent)]
pub struct VectorizableText(String);

impl VectorizableText {
    pub fn try_new(text: impl Into<String>) -> Result<Self, VectorizableTextError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(VectorizableTextError::Empty);
        }
        if !text.chars().any(char::is_alphanumeric) {
            return Err(VectorizableTextError::NoAlphanumeric);
        }
        Ok(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for VectorizableText {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VectorizableText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        assert_eq!(
            VectorizableText::try_new(""),
            Err(VectorizableTextError::Empty)
        );
    }

    #[test]
    fn rejects_pure_whitespace() {
        for s in &["   ", "\n\t\r", "\u{00A0}", " \t \n "] {
            assert_eq!(
                VectorizableText::try_new(*s),
                Err(VectorizableTextError::Empty),
                "input was {s:?}",
            );
        }
    }

    #[test]
    fn rejects_punctuation_only() {
        for s in &["!?,.;", "---", "  ?!  ", "()[]{}"] {
            assert_eq!(
                VectorizableText::try_new(*s),
                Err(VectorizableTextError::NoAlphanumeric),
                "input was {s:?}",
            );
        }
    }

    #[test]
    fn rejects_emoji_only() {
        for s in &["🚀", "🐺🌟", "👍 👎"] {
            assert_eq!(
                VectorizableText::try_new(*s),
                Err(VectorizableTextError::NoAlphanumeric),
                "input was {s:?}",
            );
        }
    }

    #[test]
    fn accepts_single_alphanumeric_chars() {
        for s in &["x", "1", "中", "Κ", "Ж"] {
            assert!(VectorizableText::try_new(*s).is_ok(), "input was {s:?}",);
        }
    }

    #[test]
    fn accepts_mixed_punctuation_and_letters() {
        assert!(VectorizableText::try_new("!hello!").is_ok());
        assert!(VectorizableText::try_new("...x...").is_ok());
    }

    #[test]
    fn accepts_non_latin_scripts() {
        for s in &["Kingsley", "Кингсли", "キングスリー", "金斯利"] {
            assert!(VectorizableText::try_new(*s).is_ok(), "input was {s:?}",);
        }
    }

    #[test]
    fn preserves_inner_string_verbatim() {
        let raw = "  hello world  ";
        let vt = VectorizableText::try_new(raw).unwrap();
        assert_eq!(vt.as_str(), raw);
        assert_eq!(vt.as_ref(), raw);
        assert_eq!(format!("{vt}"), raw);
        assert_eq!(vt.into_inner(), raw);
    }

    #[test]
    fn serialises_transparently() {
        let vt = VectorizableText::try_new("hello").unwrap();
        let json = serde_json::to_string(&vt).unwrap();
        assert_eq!(json, "\"hello\"");
    }

    #[test]
    fn metric_labels_are_stable() {
        assert_eq!(VectorizableTextError::Empty.as_metric_label(), "empty");
        assert_eq!(
            VectorizableTextError::NoAlphanumeric.as_metric_label(),
            "no_alphanumeric",
        );
    }

    #[test]
    fn error_displays_human_readable_message() {
        assert!(VectorizableTextError::Empty.to_string().contains("empty"));
        assert!(VectorizableTextError::NoAlphanumeric
            .to_string()
            .contains("alphanumeric"));
    }

    #[test]
    fn error_propagates_through_eyre_question_mark() {
        fn caller() -> eyre::Result<VectorizableText> {
            Ok(VectorizableText::try_new("")?)
        }
        let err = caller().expect_err("empty input must fail");
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn try_new_predicate_matches_documented_rule() {
        for s in [
            "hello",
            "  hello  ",
            "!hi!",
            "中",
            "1",
            "",
            "   ",
            "!?",
            "🚀",
        ] {
            let predicate_says_ok = s.chars().any(char::is_alphanumeric);
            let result_is_ok = VectorizableText::try_new(s).is_ok();
            assert_eq!(predicate_says_ok, result_is_ok, "mismatch for {s:?}");
        }
    }
}
