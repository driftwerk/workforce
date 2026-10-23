//! Glob matcher for built-in tool shadowing. Patterns come from
//! `McpServerConfig.shadows` (e.g. `["linear_*"]`) and are evaluated
//! per-request against the active user's OAuth state by
//! [`super::access::McpToolAccess`].
//!
//! ## Dot/underscore normalisation
//!
//! Built-in adapter tools register under **dotted** internal names
//! (`linear.issue_create`, `metabase.dashboard_create`) — see the
//! `unload_skill` prefix strip in `skill_loader`. The LLM wire boundary
//! later rewrites dots to underscores (`runner::to_llm_name`). Shadow
//! patterns are authored both ways in the wild: presets historically
//! used `linear_*` while the natural form is `linear.*`. To keep
//! already-persisted configs working *and* accept either style, the
//! matcher canonicalises both pattern and candidate to a single form
//! (dots → underscores) before comparing. Without this, `linear_*`
//! never matched the dotted `linear.issue_create` and shadowing
//! silently no-opped for every dotted adapter.

/// Canonical form for shadow comparison: dots collapse to underscores so
/// `linear.*`, `linear_*`, `linear.issue_create` and `linear_issue_create`
/// all reduce to the same namespace. Mirrors [`crate::runner`]'s
/// `to_llm_name` so the matcher agrees with the LLM-facing name.
fn canon(s: &str) -> String {
    s.replace('.', "_")
}

/// Returns `true` if `name` matches `pattern`. Patterns support a
/// trailing `*` wildcard (`linear_*` / `linear.*` both match
/// `linear.issue_create`) and exact match (`linear`). Dots and
/// underscores are treated as equivalent on both sides. Empty patterns
/// match nothing.
pub fn matches(pattern: &str, name: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let pattern = canon(pattern);
    let name = canon(name);
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

/// Returns `true` if `name` matches any non-empty pattern in `patterns`.
pub fn matches_any(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| matches(p, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_handles_wildcard_and_exact() {
        assert!(matches("linear_*", "linear_issue_create"));
        assert!(matches("linear_*", "linear_"));
        assert!(!matches("linear_*", "linearfoo"));
        assert!(matches("linear", "linear"));
        assert!(!matches("linear", "linear_anything"));
        assert!(!matches("", "anything"));
    }

    /// Regression: adapters register DOTTED internal names
    /// (`linear.issue_create`), but presets historically declared
    /// underscored patterns (`linear_*`). Both pattern styles must match
    /// the real dotted names, or shadowing silently no-ops.
    #[test]
    fn matches_dotted_names_with_either_pattern_style() {
        // Underscored pattern (as persisted by older presets) vs real name.
        assert!(matches("linear_*", "linear.issue_create"));
        assert!(matches("jira_*", "jira.search"));
        assert!(matches("metabase_*", "metabase.dashboard_create"));
        // Dotted pattern (the natural form) vs real name.
        assert!(matches("linear.*", "linear.issue_create"));
        assert!(matches("metabase.*", "metabase.dashboard_create"));
        // Still namespace-scoped: a different prefix must not match.
        assert!(!matches("linear.*", "linearish.tool"));
        assert!(!matches("metabase_*", "metabasefoo.tool"));
        // MCP-surfaced names are excluded by the caller's `mcp.<id>.`
        // prefix check, but the pattern itself must not greedily match a
        // sibling namespace either.
        assert!(!matches("linear.*", "jira.search"));
    }

    #[test]
    fn matches_any_short_circuits() {
        let patterns = vec!["linear_*".to_string(), "jira_*".to_string()];
        assert!(matches_any(&patterns, "linear_issue_create"));
        assert!(matches_any(&patterns, "jira_search"));
        assert!(!matches_any(&patterns, "hubspot.contact_list"));
        assert!(!matches_any(&[], "linear_anything"));
    }
}
