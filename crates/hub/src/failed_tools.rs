//! Deterministic failed-tool assertion flag (G2).
//!
//! A turn can report a confident answer while some of its tool calls failed —
//! the reader has no way to know which claims rest on actions that never
//! happened. These pure helpers derive, with no LLM involved, the set of tool
//! families whose LAST invocation failed (a later success in the family — the
//! same tool retried, or a sibling reached the same data — means the model
//! recovered) and render the compact notice the delivery sites append first.

use workforce_agent::runner::ToolInvocation;
use workforce_agent::tools::loading::{CALL_TOOL, LOAD_TOOL};

/// Bytes of error text kept per flagged tool (UTF-8 boundary safe, `…` added
/// when cut).
const ERROR_MAX_BYTES: usize = 200;

/// One tool whose final invocation this turn failed without a successful retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedTool {
    pub tool: String,
    pub error: String,
}

/// The prefix a tool shares with its siblings: `metabase.card_query` and
/// `metabase.dataset_query_native` are one family, an MCP server's tools
/// (`mcp.<server>.<tool>`, server ids taken as undotted like the gateway's
/// provenance does) are one family per server, and a bare name is its own.
pub fn tool_family(tool_name: &str) -> &str {
    let segments = if tool_name.starts_with("mcp.") { 2 } else { 1 };
    tool_name
        .match_indices('.')
        .nth(segments - 1)
        .map_or(tool_name, |(end, _)| &tool_name[..end])
}

/// The last failed call of every tool family that never recovered, in the
/// order the families were first used. A later success anywhere in the family
/// — the same tool retried, or a sibling reaching the same data — is the model
/// recovering, not a failure the reader must weigh. Invocations denied by
/// policy/approval are excluded entirely (the Sentinel path already surfaces
/// those), as are approval-gated calls: a routed approval is pending, not
/// failed — the ⏳ routing notice owns its visibility, and "failed and not
/// retried" would contradict it. A failed `tool.load` or `tool.call` is no
/// action at all: a name missed the catalogue, or the call named no tool.
pub fn unrecovered_failures(invocations: &[ToolInvocation]) -> Vec<FailedTool> {
    let considered = invocations.iter().filter(|inv| {
        inv.denial_reason.is_none()
            && inv.policy_decision != "require_approval"
            && inv.tool_name != LOAD_TOOL
            && inv.tool_name != CALL_TOOL
    });
    let mut order: Vec<&str> = Vec::new();
    let mut last: std::collections::HashMap<&str, &ToolInvocation> =
        std::collections::HashMap::new();
    for inv in considered {
        let family = tool_family(&inv.tool_name);
        if last.insert(family, inv).is_none() {
            order.push(family);
        }
    }
    order
        .into_iter()
        .filter_map(|family| last.get(family).filter(|inv| !inv.success))
        .map(|inv| FailedTool {
            tool: inv.tool_name.clone(),
            error: render_error(inv.error.as_deref()),
        })
        .collect()
}

/// A single display line's worth of error: newlines collapsed (the block is
/// one bullet per tool) and truncated to [`ERROR_MAX_BYTES`].
fn render_error(error: Option<&str>) -> String {
    let flat = error
        .unwrap_or("failed (no error detail)")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    workforce_common::text::truncate_with_ellipsis(&flat, ERROR_MAX_BYTES)
}

/// The notice appended ahead of any grounding blocks. `None` when nothing
/// failed unrecovered.
pub fn failed_tools_block(failures: &[FailedTool]) -> Option<String> {
    (!failures.is_empty()).then(|| {
        let lines = failures
            .iter()
            .map(|f| format!("• `{}`: {}", f.tool, f.error))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "⚠ *Unverified — these actions failed and were not successfully retried:*\n\
             {lines}\n\
             Claims that depend on them may be wrong."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation(tool: &str, success: bool, error: Option<&str>) -> ToolInvocation {
        ToolInvocation {
            tool_name: tool.to_string(),
            input: serde_json::Value::Null,
            output: serde_json::Value::Null,
            success,
            policy_decision: "allow".to_string(),
            duration_ms: 1,
            denial_reason: None,
            error: error.map(str::to_string),
        }
    }

    fn denied(tool: &str) -> ToolInvocation {
        ToolInvocation {
            denial_reason: Some("requires approval".to_string()),
            ..invocation(tool, false, Some("denied by policy"))
        }
    }

    /// The live case: an agent was asked to translate lyrics from a video with
    /// no video attached. `view_attachment` read the conversation, found no
    /// files, and the agent correctly answered "you didn't attach a video" —
    /// then the notice appended "these actions failed ... claims that depend on
    /// them may be wrong", contradicting an answer this very lookup had
    /// confirmed. An empty lookup is a success (`ToolResult::empty`), so it must
    /// never reach this block.
    #[test]
    fn a_lookup_that_found_nothing_is_not_a_failed_action() {
        let invocations = vec![invocation("view_attachment", true, None)];
        assert!(
            unrecovered_failures(&invocations).is_empty(),
            "finding nothing is a verified fact, not an unretried failure"
        );
        assert!(failed_tools_block(&unrecovered_failures(&invocations)).is_none());
    }

    #[test]
    fn fail_then_success_is_a_recovery_and_not_flagged() {
        let invocations = vec![
            invocation("jira.search", false, Some("timeout")),
            invocation("jira.search", true, None),
        ];
        assert!(unrecovered_failures(&invocations).is_empty());
    }

    #[test]
    fn success_then_fail_is_flagged_with_the_last_error() {
        let invocations = vec![
            invocation("jira.search", true, None),
            invocation("jira.search", false, Some("500 from server")),
        ];
        assert_eq!(
            unrecovered_failures(&invocations),
            vec![FailedTool {
                tool: "jira.search".to_string(),
                error: "500 from server".to_string(),
            }]
        );
    }

    #[test]
    fn a_sibling_tool_reaching_the_data_is_a_recovery() {
        let invocations = vec![
            invocation("metabase.card_query", false, Some("SQL error")),
            invocation("metabase.dataset_query_native", true, None),
        ];
        assert!(unrecovered_failures(&invocations).is_empty());
    }

    #[test]
    fn a_family_whose_last_call_failed_names_that_call() {
        let invocations = vec![
            invocation("metabase.card_query", true, None),
            invocation("metabase.dataset_query_native", false, Some("timeout")),
            invocation("jira.search", true, None),
        ];
        assert_eq!(
            unrecovered_failures(&invocations),
            vec![FailedTool {
                tool: "metabase.dataset_query_native".to_string(),
                error: "timeout".to_string(),
            }]
        );
    }

    #[test]
    fn independent_tools_are_grouped_separately_in_order() {
        let invocations = vec![
            invocation("a.read", false, Some("boom a")),
            invocation("b.write", true, None),
            invocation("c.list", false, Some("boom c")),
        ];
        let failures = unrecovered_failures(&invocations);
        assert_eq!(
            failures.iter().map(|f| f.tool.as_str()).collect::<Vec<_>>(),
            vec!["a.read", "c.list"]
        );
    }

    #[test]
    fn routed_pending_approvals_are_not_failures() {
        // A routed approval records success=false with NO denial_reason
        // (pending, not denied) — the ⏳ routing notice owns its visibility,
        // so the failed-tools block must not contradict it.
        let routed = ToolInvocation {
            policy_decision: "require_approval".to_string(),
            ..invocation(
                "vault.delete",
                false,
                Some("⏳ This action requires approval from a designated approver"),
            )
        };
        assert!(unrecovered_failures(&[routed]).is_empty());
    }

    #[test]
    fn a_failed_load_or_call_is_no_action_and_an_unretried_unloaded_call_is() {
        let load = ToolInvocation {
            policy_decision: "load".to_string(),
            ..invocation(LOAD_TOOL, false, Some("Unknown tool name(s): nope."))
        };
        let call = invocation(CALL_TOOL, false, Some("`tool_call` needs the `name`"));
        assert!(unrecovered_failures(&[call]).is_empty());
        let unloaded = ToolInvocation {
            policy_decision: "load".to_string(),
            ..invocation("vault.set", false, Some("`vault_set` was not loaded"))
        };
        assert!(unrecovered_failures(&[load]).is_empty());
        assert_eq!(
            unrecovered_failures(std::slice::from_ref(&unloaded)).len(),
            1
        );
        assert!(
            unrecovered_failures(&[unloaded, invocation("vault.set", true, None)]).is_empty(),
            "calling it again once loaded is the recovery"
        );
    }

    #[test]
    fn denied_invocations_are_excluded_entirely() {
        // A denial is not an unrecovered failure — and it must not shadow an
        // earlier real outcome either.
        let invocations = vec![
            invocation("x.run", false, Some("network unreachable")),
            denied("x.run"),
            denied("y.run"),
        ];
        assert_eq!(
            unrecovered_failures(&invocations),
            vec![FailedTool {
                tool: "x.run".to_string(),
                error: "network unreachable".to_string(),
            }]
        );
    }

    #[test]
    fn error_text_is_flattened_and_truncated() {
        let long = format!("line one\nline two   {}", "x".repeat(400));
        let invocations = vec![invocation("t", false, Some(&long))];
        let failures = unrecovered_failures(&invocations);
        let error = &failures[0].error;
        assert!(!error.contains('\n'), "newlines collapsed: {error}");
        assert!(error.starts_with("line one line two x"));
        assert!(error.ends_with('…'));
        assert!(error.chars().count() <= 201, "200 bytes + ellipsis");

        let bare = unrecovered_failures(&[invocation("t", false, None)]);
        assert_eq!(bare[0].error, "failed (no error detail)");
    }

    #[test]
    fn empty_failures_render_no_block() {
        assert_eq!(failed_tools_block(&[]), None);
        assert!(unrecovered_failures(&[]).is_empty());
        assert!(unrecovered_failures(&[invocation("ok", true, None)]).is_empty());
    }

    #[test]
    fn families_group_siblings_and_keep_mcp_servers_apart() {
        assert_eq!(tool_family("metabase.card_query"), "metabase");
        assert_eq!(tool_family("view_attachment"), "view_attachment");
        assert_eq!(tool_family("hello-net.fetch_url"), "hello-net");
        assert_eq!(tool_family("mcp.hubspot.discover_schema"), "mcp.hubspot");
        assert_eq!(tool_family("mcp.jira"), "mcp.jira");
        let invocations = vec![
            invocation("mcp.hubspot.contacts", false, Some("401")),
            invocation("mcp.jira.search", true, None),
        ];
        assert_eq!(
            unrecovered_failures(&invocations)
                .iter()
                .map(|f| f.tool.as_str())
                .collect::<Vec<_>>(),
            ["mcp.hubspot.contacts"]
        );
    }

    #[test]
    fn block_format_is_stable() {
        let failures = vec![
            FailedTool {
                tool: "jira.search".to_string(),
                error: "timeout".to_string(),
            },
            FailedTool {
                tool: "data.query".to_string(),
                error: "no such table".to_string(),
            },
        ];
        assert_eq!(
            failed_tools_block(&failures).unwrap(),
            "⚠ *Unverified — these actions failed and were not successfully retried:*\n\
             • `jira.search`: timeout\n\
             • `data.query`: no such table\n\
             Claims that depend on them may be wrong."
        );
    }
}
