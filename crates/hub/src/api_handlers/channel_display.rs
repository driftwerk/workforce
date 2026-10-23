//! Human-readable rendering of a session's `channel` identifier for the
//! Sessions UI.
//!
//! Session channels are opaque platform-qualified strings, e.g.
//! `slack:{workspace}:{channel_id}:{thread_ts}` (see `slack_manager.rs`), or
//! single-segment conversational channels like `web` / `ios` / `terminal`.
//! Raw, these are unreadable in the admin UI. [`parse_channel_display`] splits
//! the identifier into structured parts and resolves a friendly `name` using
//! **only** data already on the session (resolved `channel_name` metadata, or
//! the session label when it carries a resolved name) — never a live API call,
//! because the sessions list renders hundreds of rows at once.
//!
//! Cold sessions that were never name-resolved degrade gracefully to an
//! id-derived name (`#C0FAKECHAN3`); the UI keeps the full `raw` string on
//! hover so the original is always one mouse-over away.

use serde::Serialize;
use serde_json::Value;

/// Conversation kind, derived from the Slack channel-id prefix so the UI can
/// group by DM / channel / group without re-sniffing the display string.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    Dm,
    Channel,
    Group,
    Other,
}

/// Structured, UI-friendly view of a session channel.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChannelDisplay {
    /// Platform segment: `slack`, `web`, `ios`, `terminal`, `workflow`, …
    pub platform: String,
    /// Slack workspace (the second segment), when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Resolved human-readable name (`DM with Kingsley`, `#general`, `Web`),
    /// or an id-derived fallback (`#C0FAKECHAN3`) when never resolved.
    pub name: String,
    pub kind: ChannelKind,
    /// Whether this channel is a specific thread (Slack thread_ts present).
    pub thread: bool,
    /// The original, opaque channel identifier — shown on hover.
    pub raw: String,
}

/// Build a [`ChannelDisplay`] from a session's `channel`, optional `label`,
/// and `metadata` object. Pure: no I/O.
pub fn parse_channel_display(
    channel: &str,
    label: Option<&str>,
    metadata: &Value,
) -> ChannelDisplay {
    let raw = channel.to_string();
    let meta_name = metadata
        .get("channel_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    match channel.split_once(':') {
        Some(("slack", rest)) => {
            // rest = "{workspace}:{channel_id}:{thread_ts}"; trailing parts may
            // be empty or absent on older/root channels.
            let mut parts = rest.splitn(3, ':');
            let workspace = parts.next().unwrap_or("");
            let channel_id = parts.next().unwrap_or("");
            let thread_ts = parts.next().unwrap_or("");
            let kind = slack_kind(channel_id);
            ChannelDisplay {
                platform: "slack".to_string(),
                workspace: (!workspace.is_empty()).then(|| workspace.to_string()),
                name: resolve_slack_name(meta_name, label, channel, channel_id, kind),
                kind,
                thread: !thread_ts.is_empty(),
                raw,
            }
        }
        Some((platform, _)) => non_slack(platform, raw),
        None if !channel.is_empty() => non_slack(channel, raw),
        None => non_slack("unknown", raw),
    }
}

fn non_slack(platform: &str, raw: String) -> ChannelDisplay {
    ChannelDisplay {
        platform: platform.to_string(),
        workspace: None,
        name: friendly_platform_name(platform),
        kind: ChannelKind::Other,
        thread: false,
        raw,
    }
}

/// Slack channel ids are prefixed by kind: `C` public, `D` DM, `G` group/mpim.
fn slack_kind(channel_id: &str) -> ChannelKind {
    match channel_id.chars().next() {
        Some('D') => ChannelKind::Dm,
        Some('G') => ChannelKind::Group,
        Some('C') => ChannelKind::Channel,
        _ => ChannelKind::Other,
    }
}

/// Resolve the channel-name portion of a Slack channel, preferring durable
/// resolved data over an id-derived fallback.
fn resolve_slack_name(
    meta_name: Option<&str>,
    label: Option<&str>,
    channel: &str,
    channel_id: &str,
    kind: ChannelKind,
) -> String {
    if let Some(name) = meta_name {
        return name.to_string();
    }
    // A label is a resolved name only when it isn't the `{channel}:{owner}`
    // fallback stamped on unresolved sessions (that fallback starts with the
    // channel string / `slack:`).
    if let Some(l) = label {
        if !l.is_empty() && !l.starts_with("slack:") && !l.starts_with(channel) {
            return l.to_string();
        }
    }
    match kind {
        ChannelKind::Channel => format!("#{channel_id}"),
        ChannelKind::Dm => format!("DM ({channel_id})"),
        ChannelKind::Group => format!("Group ({channel_id})"),
        ChannelKind::Other if !channel_id.is_empty() => channel_id.to_string(),
        ChannelKind::Other => "unknown".to_string(),
    }
}

fn friendly_platform_name(platform: &str) -> String {
    match platform {
        "web" => "Web".to_string(),
        "ios" => "iOS".to_string(),
        "terminal" => "Terminal".to_string(),
        "rest" => "REST".to_string(),
        "workflow" => "Workflow".to_string(),
        "scheduler" => "Scheduler".to_string(),
        "" => "unknown".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => other.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty() -> Value {
        json!({})
    }

    #[test]
    fn slack_public_channel_thread_falls_back_to_id() {
        let d = parse_channel_display(
            "slack:atlas:C0FAKECHAN3:1780071080.274659",
            // The unresolved fallback label `{channel}:{owner}`.
            Some("slack:atlas:C0FAKECHAN3:1780071080.274659:69116b82-6a40"),
            &empty(),
        );
        assert_eq!(d.platform, "slack");
        assert_eq!(d.workspace.as_deref(), Some("atlas"));
        assert_eq!(d.name, "#C0FAKECHAN3");
        assert_eq!(d.kind, ChannelKind::Channel);
        assert!(d.thread);
    }

    #[test]
    fn slack_name_prefers_metadata_but_kind_comes_from_id() {
        let d = parse_channel_display(
            "slack:atlas:D0FAKEDMAA1",
            Some("slack:atlas:D0FAKEDMAA1:owner"),
            &json!({ "channel_name": "DM with Kingsley" }),
        );
        assert_eq!(d.name, "DM with Kingsley");
        assert_eq!(d.kind, ChannelKind::Dm);
        assert!(!d.thread);
        assert_eq!(d.workspace.as_deref(), Some("atlas"));
    }

    #[test]
    fn slack_group_resolved_name_keeps_group_kind() {
        // A group DM with a custom resolved name must still classify as Group,
        // not be mistaken for a DM by a name-prefix heuristic.
        let d = parse_channel_display(
            "slack:ws:G0123ABC",
            None,
            &json!({ "channel_name": "Group DM" }),
        );
        assert_eq!(d.name, "Group DM");
        assert_eq!(d.kind, ChannelKind::Group);
    }

    #[test]
    fn slack_name_uses_resolved_label_when_no_metadata() {
        let d = parse_channel_display("slack:atlas:C0FAKECHAN3", Some("#general"), &empty());
        assert_eq!(d.name, "#general");
        assert_eq!(d.kind, ChannelKind::Channel);
    }

    #[test]
    fn slack_empty_channel_name_metadata_falls_through() {
        let d = parse_channel_display("slack:ws:D0123ABC", None, &json!({ "channel_name": "" }));
        assert_eq!(d.name, "DM (D0123ABC)");
        assert_eq!(d.kind, ChannelKind::Dm);
    }

    #[test]
    fn slack_group_id_fallback() {
        let d = parse_channel_display("slack:ws:G0123ABC", None, &empty());
        assert_eq!(d.name, "Group (G0123ABC)");
        assert_eq!(d.kind, ChannelKind::Group);
    }

    #[test]
    fn non_slack_single_segment() {
        let web = parse_channel_display("web", None, &empty());
        assert_eq!(web.name, "Web");
        assert_eq!(web.kind, ChannelKind::Other);
        assert_eq!(parse_channel_display("ios", None, &empty()).name, "iOS");
        assert_eq!(
            parse_channel_display("terminal", None, &empty()).name,
            "Terminal"
        );
    }

    #[test]
    fn non_slack_multi_segment_platform_capitalises() {
        let d = parse_channel_display("discord:guild:chan", None, &empty());
        assert_eq!(d.platform, "discord");
        assert_eq!(d.name, "Discord");
        assert_eq!(d.kind, ChannelKind::Other);
        assert!(d.workspace.is_none());
    }

    #[test]
    fn empty_channel_is_unknown() {
        let d = parse_channel_display("", None, &empty());
        assert_eq!(d.platform, "unknown");
        assert_eq!(d.name, "Unknown");
        assert_eq!(d.kind, ChannelKind::Other);
    }
}
