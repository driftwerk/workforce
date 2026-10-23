//! Auto-post suppression for the Slack response path.

use workforce_agent::runner::ToolInvocation;

const MESSAGING_SEND: &str = "messaging.send";

/// Returns true iff the agent already replied to the inbound conversation
/// during the run via a successful `messaging.send` tool call — callers use
/// this to suppress the post-run auto-post that would otherwise duplicate the
/// agent's own message.
///
/// The send matches when it targets `channel` and its `thread_id` resolves to
/// the same conversation: equal to the parent `thread_ts` or the user's
/// `message_ts`. `Option` equality also covers the common case where the agent
/// omits `thread_id` on a top-level message (`thread_ts` absent, send absent),
/// while a root post against a threaded inbound stays distinct and is not
/// suppressed.
pub fn agent_already_posted_to_thread(
    invocations: &[ToolInvocation],
    channel: &str,
    thread_ts: Option<&str>,
    message_ts: Option<&str>,
) -> bool {
    invocations
        .iter()
        .filter(|inv| inv.tool_name == MESSAGING_SEND && inv.success)
        .filter(|inv| inv.input.get("channel").and_then(|v| v.as_str()) == Some(channel))
        .any(|inv| {
            let sent = inv.input.get("thread_id").and_then(|v| v.as_str());
            sent == thread_ts || sent == message_ts
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use workforce_agent::runner::ToolInvocation;

    fn inv(name: &str, input: serde_json::Value, success: bool) -> ToolInvocation {
        ToolInvocation {
            tool_name: name.to_string(),
            input,
            output: json!(null),
            success,
            policy_decision: "allowed".to_string(),
            duration_ms: 0,
            denial_reason: None,
            error: None,
        }
    }

    #[test]
    fn helper_suppresses_when_send_matches_thread_ts() {
        let invs = vec![inv(
            MESSAGING_SEND,
            json!({ "channel": "C1", "thread_id": "t1", "text": "hi" }),
            true,
        )];
        assert!(agent_already_posted_to_thread(
            &invs,
            "C1",
            Some("t1"),
            Some("m1")
        ));
    }

    #[test]
    fn helper_suppresses_when_send_matches_message_ts() {
        let invs = vec![inv(
            MESSAGING_SEND,
            json!({ "channel": "C1", "thread_id": "m1", "text": "hi" }),
            true,
        )];
        assert!(agent_already_posted_to_thread(
            &invs,
            "C1",
            None,
            Some("m1")
        ));
    }

    #[test]
    fn helper_suppresses_top_level_send_without_thread_id() {
        let invs = vec![inv(
            MESSAGING_SEND,
            json!({ "channel": "C1", "text": "hi" }),
            true,
        )];
        assert!(agent_already_posted_to_thread(
            &invs,
            "C1",
            None,
            Some("m1")
        ));
    }

    #[test]
    fn helper_allows_threaded_inbound_send_without_thread_id() {
        let invs = vec![inv(
            MESSAGING_SEND,
            json!({ "channel": "C1", "text": "hi" }),
            true,
        )];
        assert!(!agent_already_posted_to_thread(
            &invs,
            "C1",
            Some("t1"),
            Some("m1")
        ));
    }

    #[test]
    fn helper_allows_when_send_to_different_channel() {
        let invs = vec![inv(
            MESSAGING_SEND,
            json!({ "channel": "C_OTHER", "thread_id": "t1", "text": "hi" }),
            true,
        )];
        assert!(!agent_already_posted_to_thread(
            &invs,
            "C1",
            Some("t1"),
            Some("m1")
        ));
    }

    #[test]
    fn helper_allows_when_send_to_same_channel_different_thread() {
        let invs = vec![inv(
            MESSAGING_SEND,
            json!({ "channel": "C1", "thread_id": "t_OTHER", "text": "hi" }),
            true,
        )];
        assert!(!agent_already_posted_to_thread(
            &invs,
            "C1",
            Some("t1"),
            Some("m1")
        ));
    }

    #[test]
    fn helper_allows_when_messaging_send_failed() {
        let invs = vec![inv(
            MESSAGING_SEND,
            json!({ "channel": "C1", "thread_id": "t1", "text": "hi" }),
            false,
        )];
        assert!(!agent_already_posted_to_thread(
            &invs,
            "C1",
            Some("t1"),
            Some("m1")
        ));
    }

    #[test]
    fn helper_allows_when_no_invocations() {
        assert!(!agent_already_posted_to_thread(&[], "C1", Some("t1"), None));
    }

    #[test]
    fn helper_ignores_other_tools_with_matching_channel() {
        let invs = vec![inv(
            "messaging.read",
            json!({ "channel": "C1", "thread_id": "t1" }),
            true,
        )];
        assert!(!agent_already_posted_to_thread(
            &invs,
            "C1",
            Some("t1"),
            None
        ));
    }

    #[test]
    fn helper_suppresses_when_one_of_many_matches() {
        let invs = vec![
            inv("filesystem.read", json!({ "path": "/x" }), true),
            inv(
                MESSAGING_SEND,
                json!({ "channel": "C_OTHER", "thread_id": "t1", "text": "x" }),
                true,
            ),
            inv(
                MESSAGING_SEND,
                json!({ "channel": "C1", "thread_id": "t1", "text": "ok" }),
                true,
            ),
        ];
        assert!(agent_already_posted_to_thread(
            &invs,
            "C1",
            Some("t1"),
            None
        ));
    }

    #[test]
    fn helper_treats_non_string_channel_as_no_match() {
        let invs = vec![inv(
            MESSAGING_SEND,
            json!({ "channel": 42, "thread_id": "t1", "text": "hi" }),
            true,
        )];
        assert!(!agent_already_posted_to_thread(
            &invs,
            "C1",
            Some("t1"),
            None
        ));
    }
}
