//! Per-server allow/deny filtering of upstream MCP tools.
//!
//! A large MCP server (e.g. Harbor) dumps its entire API surface into
//! the agent's tool registry. An operator curates it with glob patterns on
//! [`McpServerConfig::tools_allow`] / [`McpServerConfig::tools_deny`],
//! cutting how many tools get registered (and, for a large-context model,
//! how many schemas land inline per request).
//!
//! This [`ToolAllowlist`] is the single filtering primitive. Its
//! [`ToolAllowlist::keeps_name`] is passed as the `keep` predicate to
//! [`McpClient::tools_list_filtered`] at every fetch site (`start_one`, the
//! refresh loop, `manager.refresh`), so the filter is applied **during**
//! the `tools/list` fetch — *before* the registration cap counts. That is
//! what lets a server with more tools than the cap still register its
//! allowed subset, and it means a denied tool can never slip back in on a
//! refresh.
//!
//! Patterns are full globs (`*`, `?`, `[...]`) matched against each tool's
//! **raw upstream name** — the name the server advertises (e.g.
//! `issue_create`), not the `mcp.<server_id>.` qualified form the agent
//! sees. `tools_allow` is an allowlist (empty = allow everything);
//! `tools_deny` subtracts from the result and wins ties.

use glob::Pattern;
use tracing::warn;

/// Compiled allow/deny globs for one server's upstream tools.
#[derive(Clone, Default)]
pub(super) struct ToolAllowlist {
    allow: Vec<Pattern>,
    deny: Vec<Pattern>,
}

impl ToolAllowlist {
    /// Compile the raw patterns from config. Invalid globs are logged and
    /// skipped (fail-open for that pattern) rather than stranding the
    /// whole server — interactive installs reject bad patterns up front
    /// via [`validate_patterns`], so a malformed glob only reaches here on
    /// a record persisted before that check existed.
    pub(super) fn from_config(server_id: &str, allow: &[String], deny: &[String]) -> Self {
        Self {
            allow: compile(server_id, "tools_allow", allow),
            deny: compile(server_id, "tools_deny", deny),
        }
    }

    /// True when no patterns are configured — the common case, letting
    /// callers skip work and logging entirely.
    pub(super) fn is_noop(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }

    /// Keep a tool iff it passes the allowlist (empty allow = allow all)
    /// and matches no deny pattern. Matched against the raw upstream name.
    ///
    /// This is the single filtering primitive — passed as the `keep`
    /// predicate to [`McpClient::tools_list_filtered`] at every fetch site
    /// (`start_one`, the refresh loop, `manager.refresh`) so the filter is
    /// applied **during** the fetch, before the registration cap counts.
    /// That's what lets an over-cap server register a curated subset, and
    /// it means a denied tool can't slip back in on a refresh.
    pub(super) fn keeps_name(&self, name: &str) -> bool {
        let allowed = self.allow.is_empty() || self.allow.iter().any(|p| p.matches(name));
        let denied = self.deny.iter().any(|p| p.matches(name));
        allowed && !denied
    }
}

fn compile(server_id: &str, field: &str, patterns: &[String]) -> Vec<Pattern> {
    patterns
        .iter()
        .filter_map(|p| match Pattern::new(p) {
            Ok(pat) => Some(pat),
            Err(e) => {
                warn!(
                    server = %server_id,
                    field,
                    pattern = %p,
                    error = %e,
                    "ignoring invalid MCP tool glob pattern"
                );
                None
            }
        })
        .collect()
}

/// Validate every `tools_allow` / `tools_deny` glob, returning the first
/// malformed pattern as a human-facing message. Called at interactive
/// install time so a typo'd glob is rejected immediately instead of
/// silently registering the server's full toolset.
pub(super) fn validate_patterns(allow: &[String], deny: &[String]) -> Result<(), String> {
    for (field, patterns) in [("tools_allow", allow), ("tools_deny", deny)] {
        for p in patterns {
            Pattern::new(p).map_err(|e| format!("malformed glob in `{field}`: `{p}` — {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: which of `names` the allowlist keeps, in order.
    fn kept<'a>(al: &ToolAllowlist, names: &[&'a str]) -> Vec<&'a str> {
        names.iter().copied().filter(|n| al.keeps_name(n)).collect()
    }

    #[test]
    fn empty_config_is_noop_and_keeps_all() {
        let al = ToolAllowlist::from_config("s", &[], &[]);
        assert!(al.is_noop());
        assert_eq!(kept(&al, &["a", "b"]), vec!["a", "b"]);
    }

    #[test]
    fn allow_is_an_allowlist() {
        let al = ToolAllowlist::from_config("s", &["issue_*".into(), "*_get".into()], &[]);
        assert_eq!(
            kept(
                &al,
                &[
                    "issue_create",
                    "project_get",
                    "comment_list",
                    "metabase_card_query"
                ]
            ),
            vec!["issue_create", "project_get"]
        );
    }

    #[test]
    fn deny_wins_over_allow() {
        let al = ToolAllowlist::from_config("s", &["issue_*".into()], &["issue_delete".into()]);
        assert_eq!(
            kept(&al, &["issue_create", "issue_delete"]),
            vec!["issue_create"]
        );
    }

    #[test]
    fn deny_only_subtracts_from_full_surface() {
        let al = ToolAllowlist::from_config("s", &[], &["*_delete".into()]);
        assert_eq!(
            kept(&al, &["issue_create", "issue_delete"]),
            vec!["issue_create"]
        );
    }

    #[test]
    fn invalid_glob_is_skipped_not_fatal() {
        // `[` is an unterminated character class — skipped, leaving the
        // valid pattern to act alone.
        let al = ToolAllowlist::from_config("s", &["[".into(), "issue_*".into()], &[]);
        assert_eq!(kept(&al, &["issue_create", "other"]), vec!["issue_create"]);
    }

    #[test]
    fn validate_patterns_flags_malformed() {
        assert!(validate_patterns(&["good_*".into()], &[]).is_ok());
        let err = validate_patterns(&["[bad".into()], &[]).unwrap_err();
        assert!(err.contains("tools_allow"), "got {err}");
    }
}
