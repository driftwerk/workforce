//! Redaction primitives for the LLM-facing `vault.{get,inspect}` tools.
//!
//! Built-in adapters and WASM skills bypass redaction entirely — they read
//! cleartext from the layered vault for HTTP `Authorization` headers. The
//! agent-tool boundary is the only redaction site, applied as an output
//! transform inside `crate::vault_tool::RedactionPolicy`.
//!
//! Modes: `Off` (cleartext), `Redact` (`abcd…1234`, default), `Deny`
//! (`None`). A non-secret allowlist passes routing pointers like
//! `skills/jira/default_workspace` through cleartext.

/// Behaviour for the LLM-facing vault read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionMode {
    /// Pass values through unchanged.
    Off,
    /// Redact every read except non-secret allowlisted paths.
    Redact,
    /// Refuse to return values; `get` always yields `None`.
    Deny,
}

impl RedactionMode {
    /// Parse a config string. Unknown values fall back to the secure
    /// default (`Redact`).
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "off" | "cleartext" | "raw" => Self::Off,
            "deny" => Self::Deny,
            _ => Self::Redact,
        }
    }
}

/// Match a vault path against a list of glob-ish patterns. Each pattern is
/// either a literal path, a `prefix*` (suffix wildcard), or contains
/// `*` segments that match any single path segment (`skills/*/email`).
#[derive(Default)]
pub struct PathMatcher {
    patterns: Vec<String>,
}

impl PathMatcher {
    pub fn new(patterns: impl IntoIterator<Item = String>) -> Self {
        Self {
            patterns: patterns.into_iter().collect(),
        }
    }

    /// Built-in non-secret allowlist — paths that store routing pointers
    /// or display names, not credentials. The LLM legitimately needs to
    /// read these to choose a workspace / show a configured email; they
    /// never store auth material.
    pub fn default_non_secret() -> Self {
        Self::new(
            [
                "skills/*/default_workspace",
                "skills/*/default_account",
                "skills/*/default_instance",
                "skills/*/workspaces/*/email",
                "skills/*/workspaces/*/site",
                "skills/*/workspaces/*/name",
                "skills/*/workspaces_index",
            ]
            .into_iter()
            .map(String::from),
        )
    }

    pub fn matches(&self, path: &str) -> bool {
        self.patterns.iter().any(|pat| pattern_matches(pat, path))
    }
}

fn pattern_matches(pattern: &str, path: &str) -> bool {
    let pat_segs: Vec<&str> = pattern.split('/').collect();
    let path_segs: Vec<&str> = path.split('/').collect();
    if pat_segs.len() != path_segs.len() {
        return false;
    }
    pat_segs
        .iter()
        .zip(path_segs.iter())
        .all(|(p, s)| *p == "*" || *p == *s)
}

/// What the LLM sees for a given vault read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disclosure {
    /// Value passes through unchanged. Either `mode == Off` or the path
    /// is on the non-secret allowlist.
    Cleartext,
    /// First 4 and last 4 chars (or `***` if too short).
    Redacted,
    /// `None` returned in place of the value.
    Denied,
}

/// Re-export of [`workforce_common::redact::redact_value`] for callers
/// that want the shared `abcd…1234` format without pulling in the whole
/// redact module path.
pub use workforce_common::redact::redact_value as redact;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_long_value() {
        assert_eq!(redact("abcdefghijklmnop"), "abcd…mnop");
        assert_eq!(redact("0123456789abcdefghijklmnop"), "0123…mnop");
    }

    #[test]
    fn redact_short_value() {
        assert_eq!(redact("short"), "***");
        assert_eq!(redact("0123456789abcde"), "***");
    }

    #[test]
    fn redact_empty() {
        assert_eq!(redact(""), "***");
    }

    #[test]
    fn pattern_matcher_segment_wildcard() {
        let m = PathMatcher::new(["skills/*/default_workspace".into()]);
        assert!(m.matches("skills/jira/default_workspace"));
        assert!(m.matches("skills/linear/default_workspace"));
        assert!(!m.matches("skills/default_workspace"));
        assert!(!m.matches("skills/jira/workspaces/acme/default_workspace"));
    }

    #[test]
    fn pattern_matcher_double_wildcard() {
        let m = PathMatcher::new(["skills/*/workspaces/*/email".into()]);
        assert!(m.matches("skills/jira/workspaces/acme/email"));
        assert!(m.matches("skills/linear/workspaces/foo/email"));
        assert!(!m.matches("skills/jira/workspaces/acme/api_key"));
    }

    #[test]
    fn redaction_mode_parse() {
        assert_eq!(RedactionMode::parse("off"), RedactionMode::Off);
        assert_eq!(RedactionMode::parse("Cleartext"), RedactionMode::Off);
        assert_eq!(RedactionMode::parse("redact"), RedactionMode::Redact);
        assert_eq!(RedactionMode::parse("deny"), RedactionMode::Deny);
        assert_eq!(RedactionMode::parse("nonsense"), RedactionMode::Redact);
    }
}
