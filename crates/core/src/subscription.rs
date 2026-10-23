//! The subscription a login belongs to, and the prompt asking a person to
//! connect one. In `workforce_core` because the hub raises the prompt,
//! `workforce-nats` carries it on `ChatProgress`, and
//! `workforce-gateway-client` reads it off `ServerMessage`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The subscription a login belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionKind {
    Claude,
    #[serde(rename = "chatgpt")]
    ChatGpt,
}

impl SubscriptionKind {
    pub const ALL: [Self; 2] = [Self::Claude, Self::ChatGpt];

    /// The vault key a login of this kind is stored under, at every tier.
    pub const fn vault_key(self) -> &'static str {
        match self {
            Self::Claude => "claude_code/oauth_token",
            Self::ChatGpt => "codex/auth_json",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::ChatGpt => "ChatGPT",
        }
    }

    /// The subscription whose login a vault key holds: its bare key, or
    /// that key under one user's `users/{uid}/`.
    pub fn of_vault_key(key: &str) -> Option<Self> {
        let logical = split_per_user_key(key).map_or(key, |(_, logical)| logical);
        Self::ALL
            .into_iter()
            .find(|kind| kind.vault_key() == logical)
    }
}

/// A vault key under one user's `users/{uid}/`, as the user's id and the key
/// beneath it; `None` for a key under no user.
pub fn split_per_user_key(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix("users/")?.split_once('/')
}

/// Asks the person a turn was for to connect a login it needed: which
/// subscription, on which agent, and why. It never carries a sign-in code,
/// so it can go wherever the turn's progress goes; `private` says whether
/// that is to the person alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectPrompt {
    pub kind: SubscriptionKind,
    pub agent_id: String,
    /// The hub's device-code sign-in started for the person, whose code is
    /// read only by them: in process, or through their own owner-checked
    /// route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<Uuid>,
    pub reason: String,
    pub private: bool,
    /// Whether the message that asked waits on the sign-in, answered once it
    /// completes.
    #[serde(default)]
    pub resumes: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_user_key_splits_into_user_and_key() {
        assert_eq!(
            split_per_user_key("users/u1/codex/auth_json"),
            Some(("u1", "codex/auth_json"))
        );
        assert_eq!(
            split_per_user_key("users//codex/auth_json"),
            Some(("", "codex/auth_json"))
        );
        assert_eq!(split_per_user_key("codex/auth_json"), None);
        assert_eq!(split_per_user_key("users/u1"), None);
        assert_eq!(
            SubscriptionKind::of_vault_key("users/u1/codex/auth_json"),
            Some(SubscriptionKind::ChatGpt)
        );
        assert_eq!(
            SubscriptionKind::of_vault_key("claude_code/oauth_token"),
            Some(SubscriptionKind::Claude)
        );
    }
}
