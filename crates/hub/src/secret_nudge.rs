//! User-facing confirmations and cleanup prompts for secret tokenization.
//!
//! Two prompts, fired from different points in the pipeline:
//!
//!  * [`post_capture_confirmation`] — fired by the ingress layer
//!    (slack_manager) the moment a credential is detected and
//!    tokenized in a user message. Tells the user "we captured your
//!    secret safely, please delete the original message" so the
//!    cleartext doesn't linger in Slack's storage. Visible in-thread.
//!
//!  * [`post_consumed_followup`] — fired by the tool-substitution
//!    layer (secret_sinks) once a secret-sink tool consumes a marker
//!    and stores the cleartext in its destination (vault, account
//!    config, slack-app config). Confirms storage and reminds the user
//!    to delete the source if they haven't yet.
//!
//! Slack permission constraint: the bot can't delete a user's own
//! message via the Web API (only admins can do that for non-bot
//! authors), so the best UX we can offer is a permalink and a clear
//! ask. We deliberately don't paste a `chat.delete` button — it would
//! either no-op or require elevated bot perms most workspaces don't
//! grant.

use workforce_common::channels::{ChannelSurface, OutboundMessage};
use workforce_common::secret_marker::{CapturedSecret, ConsumedSecret, SecretSource};

/// Build a Slack message permalink from the Slack team id (`T…`, the
/// bot's home workspace as `auth.test` names it), channel, and message
/// ts. Falls back to a plain "(see message above)" line when the team
/// id is unknown — before `auth.test` has answered, or on a surface
/// that never asked. The hub's own workspace key (the binding name in
/// `workforce.toml`) is not a Slack id and never goes in the link.
///
/// Format: `https://app.slack.com/client/<team_id>/<channel>/p<ts_no_dot>`
/// (the `p…` form is what the Slack UI generates for "Copy link").
fn slack_permalink(team_id: Option<&str>, channel: &str, message_ts: &str) -> Option<String> {
    let team_id = team_id.filter(|team| !team.is_empty())?;
    if channel.is_empty() || message_ts.is_empty() {
        return None;
    }
    let ts_compact = message_ts.replace('.', "");
    Some(format!(
        "https://app.slack.com/client/{team_id}/{channel}/p{ts_compact}"
    ))
}

/// Render the body of the capture-confirmation nudge. Single message
/// regardless of how many secrets were captured in the same paste —
/// the user only needs one prompt to act on the source message.
fn render_capture_body(captures: &[CapturedSecret], team_id: Option<&str>) -> String {
    debug_assert!(!captures.is_empty(), "caller filters empty captures");

    let labels: Vec<String> = captures
        .iter()
        .map(|c| format!("`{}` (`{}`)", c.label, c.preview))
        .collect();
    let labels_line = labels.join(", ");

    // Take the first capture's source for the permalink — captures in
    // the same call all share the same source (they came from one
    // message).
    let permalink = match &captures[0].source {
        SecretSource::Slack {
            channel,
            message_ts,
            ..
        } => slack_permalink(team_id, channel, message_ts),
        _ => None,
    };

    let cleanup_line = match permalink {
        Some(link) => format!(
            "Please delete the original message when you're done — it still contains the cleartext in Slack: {link}"
        ),
        None => {
            "Please delete the original message above when you're done — it still contains the cleartext in Slack.".to_string()
        }
    };

    format!(
        ":closed_lock_with_key: Captured {labels_line} securely as a placeholder. The agent sees only the redacted preview, never the cleartext.\n\n{cleanup_line}"
    )
}

/// Post the capture-confirmation nudge to the originating thread.
/// Idempotent failure-handling: if the surface refuses (DMs without
/// `chat:write`, channel without bot membership, transient API blip),
/// we log and move on — the audit trail in the cache still records
/// the capture, and the cleartext is already safely tokenized.
pub async fn post_capture_confirmation(
    surface: &dyn ChannelSurface,
    channel: &str,
    thread_id: Option<&str>,
    captures: &[CapturedSecret],
    team_id: Option<&str>,
) {
    if captures.is_empty() {
        return;
    }
    let text = render_capture_body(captures, team_id);
    let result = surface
        .send_message(&OutboundMessage {
            channel: channel.to_string(),
            text,
            thread_id: thread_id.map(str::to_string),
            username: None,
            icon_url: None,
            blocks: None,
        })
        .await;
    if let Err(e) = result {
        tracing::warn!(
            target: "sentinel.audit",
            channel = %channel,
            error = %e,
            "failed to post secret-capture confirmation"
        );
    }
}

/// Post the post-consume followup nudge — fired once a secret-sink
/// tool successfully consumes a marker. Tells the user the secret
/// reached its destination and reminds them to delete the source.
///
/// Only fires for `SecretSource::Slack` (other sources don't have a
/// user-deletable origin).
pub async fn post_consumed_followup(
    surface: &dyn ChannelSurface,
    channel: &str,
    thread_id: Option<&str>,
    consumed: &ConsumedSecret,
    destination_tool: &str,
    team_id: Option<&str>,
) {
    let permalink = match &consumed.source {
        SecretSource::Slack {
            channel,
            message_ts,
            ..
        } => slack_permalink(team_id, channel, message_ts),
        _ => return, // non-Slack source has no deletable origin
    };

    let body = match permalink {
        Some(link) => format!(
            ":white_check_mark: Stored your `{}` (`{}`) via `{}`. The original message still contains the cleartext — please delete it now: {}",
            consumed.label, consumed.preview, destination_tool, link
        ),
        None => format!(
            ":white_check_mark: Stored your `{}` (`{}`) via `{}`. Please delete the original message containing the cleartext.",
            consumed.label, consumed.preview, destination_tool
        ),
    };

    let result = surface
        .send_message(&OutboundMessage {
            channel: channel.to_string(),
            text: body,
            thread_id: thread_id.map(str::to_string),
            username: None,
            icon_url: None,
            blocks: None,
        })
        .await;
    if let Err(e) = result {
        tracing::warn!(
            target: "sentinel.audit",
            channel = %channel,
            error = %e,
            "failed to post secret-consumed followup"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::secret_marker::MarkerId;

    fn make_capture(source: SecretSource) -> CapturedSecret {
        CapturedSecret {
            id: MarkerId::fresh(),
            label: "ANTHROPIC_KEY".into(),
            preview: "sk-a...wXyZ".into(),
            source,
        }
    }

    #[test]
    fn permalink_constructed_when_all_fields_present() {
        let link = slack_permalink(Some("T01ABCDEF"), "C01XYZ123", "1700000000.123456");
        assert_eq!(
            link.as_deref(),
            Some("https://app.slack.com/client/T01ABCDEF/C01XYZ123/p1700000000123456")
        );
    }

    #[test]
    fn permalink_dropped_when_team_missing() {
        assert!(slack_permalink(None, "C01", "1.0").is_none());
        assert!(slack_permalink(Some(""), "C01", "1.0").is_none());
        assert!(slack_permalink(Some("T1"), "", "1.0").is_none());
        assert!(slack_permalink(Some("T1"), "C01", "").is_none());
    }

    #[test]
    fn capture_body_includes_label_and_preview() {
        let cap = make_capture(SecretSource::Slack {
            workspace_id: "T1".into(),
            channel: "C1".into(),
            message_ts: "1.0".into(),
            user: "U1".into(),
        });
        let body = render_capture_body(&[cap], Some("T1"));
        assert!(body.contains("ANTHROPIC_KEY"));
        assert!(body.contains("sk-a...wXyZ"));
        assert!(body.contains("https://app.slack.com/client/T1/C1/p10"));
    }

    #[test]
    fn capture_link_uses_the_slack_team_id_not_the_hub_workspace_key() {
        // The source's `workspace_id` is the hub's binding name for the
        // workspace ("driftwerk"), which Slack does not know; the link
        // is built from the team id `auth.test` gave the surface.
        let from_driftwerk = || {
            make_capture(SecretSource::Slack {
                workspace_id: "driftwerk".into(),
                channel: "C1".into(),
                message_ts: "1700000000.123456".into(),
                user: "U1".into(),
            })
        };
        let body = render_capture_body(&[from_driftwerk()], Some("T_HOME"));
        assert!(
            body.contains("https://app.slack.com/client/T_HOME/C1/p1700000000123456"),
            "{body}"
        );
        assert!(!body.contains("/client/driftwerk/"), "{body}");

        // No team id known yet: no link rather than a broken one.
        let body = render_capture_body(&[from_driftwerk()], None);
        assert!(!body.contains("http"), "{body}");
        assert!(body.to_lowercase().contains("delete"));
    }

    #[test]
    fn capture_body_handles_multiple_captures_in_one_message() {
        let cap1 = CapturedSecret {
            id: MarkerId::fresh(),
            label: "GITHUB_PAT".into(),
            preview: "ghp_...AbCd".into(),
            source: SecretSource::Slack {
                workspace_id: "T1".into(),
                channel: "C1".into(),
                message_ts: "1.0".into(),
                user: "U1".into(),
            },
        };
        let cap2 = CapturedSecret {
            id: MarkerId::fresh(),
            label: "ANTHROPIC_KEY".into(),
            preview: "sk-a...wXyZ".into(),
            source: SecretSource::Slack {
                workspace_id: "T1".into(),
                channel: "C1".into(),
                message_ts: "1.0".into(),
                user: "U1".into(),
            },
        };
        let body = render_capture_body(&[cap1, cap2], Some("T1"));
        assert!(body.contains("GITHUB_PAT"));
        assert!(body.contains("ANTHROPIC_KEY"));
        assert!(body.contains("ghp_...AbCd"));
        assert!(body.contains("sk-a...wXyZ"));
    }

    #[test]
    fn capture_body_falls_back_when_no_permalink() {
        let cap = make_capture(SecretSource::WebGateway { user: "U1".into() });
        let body = render_capture_body(&[cap], Some("T1"));
        // No URL, but still includes a clear instruction.
        assert!(!body.contains("http"));
        assert!(body.to_lowercase().contains("delete"));
    }
}
