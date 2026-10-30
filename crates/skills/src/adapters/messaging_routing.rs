//! Shared routing plumbing used by `messaging.*` and `reactions.*` tools.
//!
//! Centralises three concerns that both families need: building a
//! [`ChannelHint`] from the tool input + task-locals, picking the
//! platform name, and invalidating the route cache when Slack signals
//! that a channel is no longer reachable from the chosen workspace.

use tokio::sync::RwLock;
use workforce_agent::runner::{
    CURRENT_CHANNEL_ID, CURRENT_PLATFORM_USER_ID, CURRENT_USER_ID, CURRENT_WORKSPACE_ID,
};
use workforce_common::channels::{ChannelHint, ChannelSurface, SurfaceRegistry};

pub const DEFAULT_PLATFORM: &str = "slack";

pub fn channel_hint_from_input(input: &serde_json::Value) -> ChannelHint {
    let task_local = |key: &'static tokio::task::LocalKey<String>| {
        key.try_with(|v| v.clone()).ok().filter(|s| !s.is_empty())
    };
    ChannelHint::from_parts(
        input["workspace_id"].as_str().map(str::to_string),
        task_local(&CURRENT_WORKSPACE_ID),
        task_local(&CURRENT_CHANNEL_ID),
    )
}

/// The platform a messaging call names, else the one the run is on: the
/// Desk conversation a Desk run answers to, otherwise Slack.
pub fn platform_arg(input: &serde_json::Value) -> &str {
    input["platform"].as_str().unwrap_or(default_platform())
}

fn default_platform() -> &'static str {
    match workforce_agent::runner::current_desk() {
        Some(_) => workforce_common::channels::desk::DESK_PLATFORM,
        None => DEFAULT_PLATFORM,
    }
}

/// The channel a posting call names; with none, the current conversation
/// when it posts to Desk (`""`), else `None`.
pub fn channel_arg(input: &serde_json::Value) -> Option<&str> {
    input["channel"].as_str().or_else(|| {
        (platform_arg(input) == workforce_common::channels::desk::DESK_PLATFORM).then_some("")
    })
}

/// What a messaging call that posts does to its surface: writes to the
/// channel it names on the platform it resolves to.
pub fn writes(input: &serde_json::Value) -> workforce_agent::tools::SurfaceEffect {
    workforce_agent::tools::SurfaceEffect::Writes {
        platform: platform_arg(input).to_string(),
        destination: input["channel"]
            .as_str()
            .or(input["user"].as_str())
            .unwrap_or_default()
            .to_string(),
    }
}

/// Cross-channel read guard for `messaging.read` / `view_attachment`. A tool may
/// read a channel OTHER than the current conversation only when the REQUESTING
/// USER (not just the bot) is a member of it — otherwise a user could exfiltrate
/// any channel the bot happens to sit in. Reading the current conversation is
/// always allowed. Fail-closed: refuses when the requester's platform id is
/// unknown (web / non-Slack / unresolved) or the surface can't verify
/// membership. Returns a user-facing reason string on refusal.
pub async fn ensure_channel_read_access(
    surface: &dyn ChannelSurface,
    channel: &str,
) -> Result<(), String> {
    let task_local = |key: &'static tokio::task::LocalKey<String>| {
        key.try_with(|v| v.clone()).ok().filter(|s| !s.is_empty())
    };
    // Same conversation the requester is already in — never a foreign read.
    if task_local(&CURRENT_CHANNEL_ID).as_deref() == Some(channel) {
        return Ok(());
    }
    // No requesting human in scope (CLI / scheduled / system run) — operator-
    // trusted, no user to protect. A human turn always has CURRENT_USER_ID set,
    // so this never relaxes the gate for an actual user.
    if task_local(&CURRENT_USER_ID).is_none() {
        return Ok(());
    }
    let Some(requester) = task_local(&CURRENT_PLATFORM_USER_ID) else {
        return Err(format!(
            "refused: cannot read another channel ({channel}) — your account isn't linked \
             to this platform, so your access to it can't be verified"
        ));
    };
    match surface.is_channel_member(channel, &requester).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "refused: you are not a member of channel {channel}"
        )),
        Err(e) => Err(format!(
            "refused: couldn't verify your access to channel {channel}: {e}"
        )),
    }
}

/// Slack-style error strings that indicate the bot cannot reach the
/// channel from the workspace we just routed to. Matching is
/// case-insensitive because different surface layers stringify errors
/// differently.
fn looks_like_channel_not_found(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("channel_not_found")
        || m.contains("not a member of channel")
        || m.contains("not_in_channel")
}

pub async fn invalidate_if_channel_not_found(
    surfaces: &RwLock<SurfaceRegistry>,
    input: &serde_json::Value,
    channel: &str,
    err_message: &str,
) {
    if looks_like_channel_not_found(err_message) {
        surfaces
            .read()
            .await
            .routes()
            .invalidate(platform_arg(input), channel)
            .await;
    }
}

pub fn schema_workspace_property() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "description": "Workspace identifier (rarely needed — the system auto-detects which workspace the channel belongs to)"
    })
}

pub fn schema_platform_property() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "description": "Platform (slack, desk, etc.). Defaults to the Desk conversation in a \
                        Desk conversation, else slack."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_platform_defaults_to_desk_in_a_desk_conversation_and_slack_elsewhere() {
        use workforce_common::channels::desk::{DeskPlace, DeskScope, DeskTarget};
        let input = serde_json::json!({"text": "hi"});
        assert_eq!(platform_arg(&input), "slack");
        let scope = DeskScope {
            target: DeskTarget::of_turn(&DeskPlace::Dm, "desk", None, false),
            attended: true,
        };
        let on_desk = workforce_agent::runner::with_desk(Some(scope.clone()), async {
            platform_arg(&input).to_string()
        })
        .await;
        assert_eq!(on_desk, "desk");
        let named = workforce_agent::runner::with_desk(Some(scope), async {
            platform_arg(&serde_json::json!({"platform": "slack"})).to_string()
        })
        .await;
        assert_eq!(named, "slack");
    }

    #[test]
    fn channel_not_found_variants_all_match() {
        assert!(looks_like_channel_not_found("channel_not_found"));
        assert!(looks_like_channel_not_found("Channel_Not_Found"));
        assert!(looks_like_channel_not_found(
            "Slack returned channel_not_found"
        ));
        assert!(looks_like_channel_not_found(
            "bot is not a member of channel C123"
        ));
        assert!(looks_like_channel_not_found("not_in_channel"));
    }

    #[test]
    fn unrelated_errors_dont_match() {
        assert!(!looks_like_channel_not_found("rate_limited"));
        assert!(!looks_like_channel_not_found("invalid_auth"));
        assert!(!looks_like_channel_not_found(""));
    }

    #[test]
    fn platform_arg_defaults_to_slack() {
        assert_eq!(platform_arg(&serde_json::json!({})), "slack");
        assert_eq!(
            platform_arg(&serde_json::json!({"platform": "discord"})),
            "discord"
        );
    }

    #[test]
    fn channel_hint_pulls_workspace_id_from_input() {
        let hint = channel_hint_from_input(&serde_json::json!({"workspace_id": "candy"}));
        assert_eq!(hint.explicit_workspace.as_deref(), Some("candy"));
    }

    #[test]
    fn channel_hint_no_task_locals_when_called_outside_run() {
        let hint = channel_hint_from_input(&serde_json::json!({}));
        assert!(hint.explicit_workspace.is_none());
        assert!(hint.context_workspace.is_none());
        assert!(hint.context_channel.is_none());
    }
}
