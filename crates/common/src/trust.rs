//! Trust taxonomy and content classification for the sentinel security system.
//!
//! Lives in `workforce-common` so it can be referenced from persistence types
//! (e.g. `SessionMessage`) without dragging the agent runtime into the common
//! crate's dependency tree.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::sentinel_config::SentinelConfig;

/// Trust level for incoming content. Ordered: System > Owner > Paired > Untrusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    Untrusted = 0,
    Paired = 1,
    Owner = 2,
    System = 3,
}

impl std::fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrustLevel::Untrusted => write!(f, "untrusted"),
            TrustLevel::Paired => write!(f, "paired"),
            TrustLevel::Owner => write!(f, "owner"),
            TrustLevel::System => write!(f, "system"),
        }
    }
}

/// Source of incoming content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentSource {
    /// Identity files, policy rules, system prompts.
    System,
    /// Slack message.
    Slack { user_id: String, channel: String },
    /// GitHub content (PR description, comment, issue body, etc.).
    Github { repo: String, kind: String },
    /// Linear ticket content.
    Linear { identifier: String },
    /// Gateway (WebSocket / REST) message.
    Gateway { device_id: Option<String> },
    /// Web content fetched via web.search / web.fetch.
    Web { url: String },
    /// Webhook payload.
    Webhook { source: String },
}

/// Wraps any content with trust metadata.
#[derive(Debug, Clone)]
pub struct ContentEnvelope {
    pub trust_level: TrustLevel,
    pub source: ContentSource,
    pub sender_id: Option<String>,
    pub raw_content: String,
    pub timestamp: DateTime<Utc>,
}

/// Classify trust level for a given source and sender.
///
/// - System source → System trust
/// - Owner IDs (matched against config) → Owner trust
/// - Sender on pairing allowlist → Paired trust
/// - Everything else → Untrusted
pub fn classify_trust(
    source: &ContentSource,
    sender_id: Option<&str>,
    config: &SentinelConfig,
    is_on_allowlist: bool,
) -> TrustLevel {
    classify_trust_with_role(source, sender_id, config, is_on_allowlist, None)
}

/// Classify trust with an optional identity role override.
///
/// When `identity_role` is provided (from the identity system), it takes
/// precedence over the static `owner_slack_ids` / `owner_device_ids` lists:
/// - `"system_admin"` or `"manager"` → Owner trust
/// - `"member"` → Paired trust
pub fn classify_trust_with_role(
    source: &ContentSource,
    sender_id: Option<&str>,
    config: &SentinelConfig,
    is_on_allowlist: bool,
    identity_role: Option<&str>,
) -> TrustLevel {
    match source {
        ContentSource::System => TrustLevel::System,
        ContentSource::Slack { user_id, .. } => {
            if let Some(role) = identity_role {
                return role_to_trust(role);
            }
            if config.owner_slack_ids.iter().any(|id| id == user_id) {
                TrustLevel::Owner
            } else if is_on_allowlist {
                TrustLevel::Paired
            } else {
                TrustLevel::Untrusted
            }
        }
        ContentSource::Gateway { device_id } => {
            if let Some(role) = identity_role {
                return role_to_trust(role);
            }
            if let Some(did) = device_id {
                if config.owner_device_ids.iter().any(|id| id == did) {
                    TrustLevel::Owner
                } else if is_on_allowlist {
                    TrustLevel::Paired
                } else {
                    TrustLevel::Untrusted
                }
            } else if is_on_allowlist {
                TrustLevel::Paired
            } else {
                TrustLevel::Untrusted
            }
        }
        ContentSource::Web { .. }
        | ContentSource::Github { .. }
        | ContentSource::Linear { .. }
        | ContentSource::Webhook { .. } => {
            let _ = sender_id;
            TrustLevel::Untrusted
        }
    }
}

fn role_to_trust(role: &str) -> TrustLevel {
    match role {
        "system_admin" | "manager" => TrustLevel::Owner,
        "member" => TrustLevel::Paired,
        _ => TrustLevel::Untrusted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_config() -> SentinelConfig {
        SentinelConfig::default()
    }

    fn owner_config() -> SentinelConfig {
        SentinelConfig {
            owner_slack_ids: vec!["U_OWNER".to_string()],
            owner_device_ids: vec!["device-owner-1".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn classify_system_content() {
        let trust = classify_trust(&ContentSource::System, None, &empty_config(), false);
        assert_eq!(trust, TrustLevel::System);
    }

    #[test]
    fn classify_owner_slack() {
        let source = ContentSource::Slack {
            user_id: "U_OWNER".to_string(),
            channel: "C_GENERAL".to_string(),
        };
        let trust = classify_trust(&source, Some("U_OWNER"), &owner_config(), false);
        assert_eq!(trust, TrustLevel::Owner);
    }

    #[test]
    fn classify_paired_slack() {
        let source = ContentSource::Slack {
            user_id: "U_PAIRED".to_string(),
            channel: "C_GENERAL".to_string(),
        };
        let trust = classify_trust(&source, Some("U_PAIRED"), &owner_config(), true);
        assert_eq!(trust, TrustLevel::Paired);
    }

    #[test]
    fn classify_untrusted_slack() {
        let source = ContentSource::Slack {
            user_id: "U_RANDOM".to_string(),
            channel: "C_GENERAL".to_string(),
        };
        let trust = classify_trust(&source, Some("U_RANDOM"), &owner_config(), false);
        assert_eq!(trust, TrustLevel::Untrusted);
    }

    #[test]
    fn classify_web_always_untrusted() {
        let source = ContentSource::Web {
            url: "https://example.com".to_string(),
        };
        let trust = classify_trust(&source, Some("U_OWNER"), &owner_config(), true);
        assert_eq!(trust, TrustLevel::Untrusted);
    }

    #[test]
    fn classify_github_always_untrusted() {
        let source = ContentSource::Github {
            repo: "org/repo".to_string(),
            kind: "pr_description".to_string(),
        };
        let trust = classify_trust(&source, None, &owner_config(), true);
        assert_eq!(trust, TrustLevel::Untrusted);
    }

    #[test]
    fn classify_linear_always_untrusted() {
        let source = ContentSource::Linear {
            identifier: "DW-123".to_string(),
        };
        let trust = classify_trust(&source, None, &owner_config(), true);
        assert_eq!(trust, TrustLevel::Untrusted);
    }

    #[test]
    fn classify_webhook_always_untrusted() {
        let source = ContentSource::Webhook {
            source: "stripe".to_string(),
        };
        let trust = classify_trust(&source, None, &owner_config(), true);
        assert_eq!(trust, TrustLevel::Untrusted);
    }

    #[test]
    fn classify_owner_gateway() {
        let source = ContentSource::Gateway {
            device_id: Some("device-owner-1".to_string()),
        };
        let trust = classify_trust(&source, None, &owner_config(), false);
        assert_eq!(trust, TrustLevel::Owner);
    }

    #[test]
    fn classify_paired_gateway() {
        let source = ContentSource::Gateway {
            device_id: Some("device-other".to_string()),
        };
        let trust = classify_trust(&source, None, &owner_config(), true);
        assert_eq!(trust, TrustLevel::Paired);
    }

    #[test]
    fn classify_untrusted_gateway_no_device() {
        let source = ContentSource::Gateway { device_id: None };
        let trust = classify_trust(&source, None, &owner_config(), false);
        assert_eq!(trust, TrustLevel::Untrusted);
    }

    #[test]
    fn classify_gateway_no_device_but_on_allowlist() {
        let source = ContentSource::Gateway { device_id: None };
        let trust = classify_trust(&source, None, &owner_config(), true);
        assert_eq!(trust, TrustLevel::Paired);
    }

    #[test]
    fn trust_level_ordering() {
        assert!(TrustLevel::System > TrustLevel::Owner);
        assert!(TrustLevel::Owner > TrustLevel::Paired);
        assert!(TrustLevel::Paired > TrustLevel::Untrusted);
        assert!(TrustLevel::System > TrustLevel::Untrusted);
    }
}
