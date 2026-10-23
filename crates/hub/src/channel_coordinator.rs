use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info};

/// Identifies an agent in a channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelAgentId {
    /// The agent's unique identifier (e.g., "allan", "reviewer").
    pub agent_id: String,
    /// The Slack bot user ID for this agent (e.g., "U0ABCDEFGH").
    pub bot_user_id: String,
    /// Display name for plain-text @mention matching (e.g., "Brian").
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Configuration for a shared channel with multiple agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedChannelConfig {
    /// The channel ID (e.g., Slack channel ID).
    pub channel_id: String,
    /// Agents that participate in this channel.
    pub agents: Vec<ChannelAgentId>,
    /// Optional default agent that handles unroutable messages.
    #[serde(default)]
    pub default_agent: Option<String>,
    /// Claim timeout — how long to wait before assuming no other agent will claim.
    #[serde(default = "default_claim_timeout_ms")]
    pub claim_timeout_ms: u64,
}

fn default_claim_timeout_ms() -> u64 {
    500
}

/// The result of trying to claim a message for response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimResult {
    /// This agent should respond.
    Claimed,
    /// Another agent was explicitly mentioned — don't respond.
    MentionedOther(String),
    /// Another agent already claimed this message.
    AlreadyClaimed(String),
    /// No agents are registered for this channel — respond freely.
    Uncoordinated,
}

/// A pending claim on a message.
#[derive(Debug, Clone)]
struct MessageClaim {
    agent_id: String,
    claimed_at: std::time::Instant,
}

/// Coordinates which agent responds in shared channels.
///
/// When multiple agents share a Slack channel, this prevents duplicate
/// responses by implementing a claim-based protocol:
///
/// 1. If a specific agent is @mentioned, only that agent responds.
/// 2. If no agent is mentioned, the first agent to claim the message wins.
/// 3. If a default agent is configured, it gets priority for unclaimed messages.
pub struct ChannelCoordinator {
    /// Channel ID → shared config.
    channels: HashMap<String, SharedChannelConfig>,
    /// Message claims: "channel:message_ts" → claim.
    claims: Arc<Mutex<HashMap<String, MessageClaim>>>,
    /// Max age for claims before cleanup.
    claim_ttl: Duration,
}

impl ChannelCoordinator {
    /// Create a new coordinator with the given shared channel configs.
    pub fn new(configs: Vec<SharedChannelConfig>) -> Self {
        let channels: HashMap<String, SharedChannelConfig> = configs
            .into_iter()
            .map(|c| (c.channel_id.clone(), c))
            .collect();

        info!(channels = channels.len(), "Channel coordinator initialized");

        Self {
            channels,
            claims: Arc::new(Mutex::new(HashMap::new())),
            claim_ttl: Duration::from_secs(300), // 5 min TTL for claim entries
        }
    }

    /// Create an empty coordinator (no shared channels).
    pub fn empty() -> Self {
        Self {
            channels: HashMap::new(),
            claims: Arc::new(Mutex::new(HashMap::new())),
            claim_ttl: Duration::from_secs(300),
        }
    }

    /// Check if a channel is coordinated (has multiple agents).
    pub fn is_coordinated(&self, channel_id: &str) -> bool {
        self.channels.contains_key(channel_id)
    }

    /// Determine which agent (if any) is mentioned in the message text.
    /// Checks Slack-formatted mentions `<@BOT_USER_ID>` first, then falls back
    /// to plain-text `@display_name` matching (case-insensitive).
    pub fn mentioned_agent(&self, channel_id: &str, text: &str) -> Option<String> {
        let config = self.channels.get(channel_id)?;

        // First: Slack-formatted mentions <@BOT_USER_ID>
        let mentioned: Vec<&ChannelAgentId> = config
            .agents
            .iter()
            .filter(|a| text.contains(&format!("<@{}>", a.bot_user_id)))
            .collect();

        if mentioned.len() == 1 {
            return Some(mentioned[0].agent_id.clone());
        }

        // Fallback: plain-text @display_name (case-insensitive)
        let text_lower = text.to_lowercase();
        let display_mentioned: Vec<&ChannelAgentId> = config
            .agents
            .iter()
            .filter(|a| {
                a.display_name
                    .as_ref()
                    .is_some_and(|name| text_lower.contains(&format!("@{}", name.to_lowercase())))
            })
            .collect();

        if display_mentioned.len() == 1 {
            return Some(display_mentioned[0].agent_id.clone());
        }

        None
    }

    /// Whether a message in `channel_id` is `agent_id`'s alone to answer:
    /// any message outside a shared channel, or one in it naming that agent.
    pub fn addressed_to(&self, channel_id: &str, text: &str, agent_id: &str) -> bool {
        !self.is_coordinated(channel_id)
            || self.mentioned_agent(channel_id, text).as_deref() == Some(agent_id)
    }

    /// Try to claim a message for a specific agent.
    ///
    /// Returns `ClaimResult` indicating whether this agent should respond.
    pub async fn try_claim(
        &self,
        channel_id: &str,
        message_ts: &str,
        agent_id: &str,
        message_text: &str,
    ) -> ClaimResult {
        // If channel isn't coordinated, respond freely
        let config = match self.channels.get(channel_id) {
            Some(c) => c,
            None => return ClaimResult::Uncoordinated,
        };

        // Check if a specific agent is mentioned
        if let Some(mentioned) = self.mentioned_agent(channel_id, message_text) {
            if mentioned == agent_id {
                // This agent is mentioned — claim it
                let key = format!("{}:{}", channel_id, message_ts);
                let mut claims = self.claims.lock().await;
                claims.insert(
                    key,
                    MessageClaim {
                        agent_id: agent_id.to_string(),
                        claimed_at: std::time::Instant::now(),
                    },
                );
                return ClaimResult::Claimed;
            } else {
                // Another agent is mentioned — stand down
                return ClaimResult::MentionedOther(mentioned);
            }
        }

        // No specific mention — try to claim
        let key = format!("{}:{}", channel_id, message_ts);
        let mut claims = self.claims.lock().await;

        // Clean up old claims
        self.cleanup_claims(&mut claims);

        if let Some(existing) = claims.get(&key) {
            if existing.agent_id == agent_id {
                return ClaimResult::Claimed;
            }
            return ClaimResult::AlreadyClaimed(existing.agent_id.clone());
        }

        // If there's a default agent and this isn't it, defer
        if let Some(ref default) = config.default_agent {
            if default != agent_id {
                debug!(
                    agent = agent_id,
                    default = default.as_str(),
                    "Deferring to default agent"
                );
                return ClaimResult::AlreadyClaimed(default.clone());
            }
        }

        // Claim it
        claims.insert(
            key,
            MessageClaim {
                agent_id: agent_id.to_string(),
                claimed_at: std::time::Instant::now(),
            },
        );

        ClaimResult::Claimed
    }

    /// Remove expired claims.
    fn cleanup_claims(&self, claims: &mut HashMap<String, MessageClaim>) {
        let cutoff = std::time::Instant::now() - self.claim_ttl;
        claims.retain(|_, claim| claim.claimed_at > cutoff);
    }

    /// Get the list of other agents in the same channel.
    pub fn other_agents(&self, channel_id: &str, my_agent_id: &str) -> Vec<ChannelAgentId> {
        self.channels
            .get(channel_id)
            .map(|c| {
                c.agents
                    .iter()
                    .filter(|a| a.agent_id != my_agent_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Record that an agent responded (for awareness tracking).
    pub async fn record_response(&self, channel_id: &str, message_ts: &str, agent_id: &str) {
        if !self.is_coordinated(channel_id) {
            return;
        }
        let key = format!("{}:{}", channel_id, message_ts);
        let mut claims = self.claims.lock().await;
        claims.insert(
            key,
            MessageClaim {
                agent_id: agent_id.to_string(),
                claimed_at: std::time::Instant::now(),
            },
        );
        debug!(
            channel = channel_id,
            agent = agent_id,
            "Recorded agent response"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> SharedChannelConfig {
        SharedChannelConfig {
            channel_id: "C123".to_string(),
            agents: vec![
                ChannelAgentId {
                    agent_id: "allan".to_string(),
                    bot_user_id: "UBOT1".to_string(),
                    display_name: Some("Allan".to_string()),
                },
                ChannelAgentId {
                    agent_id: "reviewer".to_string(),
                    bot_user_id: "UBOT2".to_string(),
                    display_name: Some("Reviewer".to_string()),
                },
                ChannelAgentId {
                    agent_id: "helper".to_string(),
                    bot_user_id: "UBOT3".to_string(),
                    display_name: Some("Helper".to_string()),
                },
            ],
            default_agent: None,
            claim_timeout_ms: 500,
        }
    }

    #[test]
    fn a_message_is_one_agent_s_alone_outside_a_shared_channel_or_when_it_names_it() {
        let coordinator = ChannelCoordinator::new(vec![test_config()]);
        assert!(coordinator.addressed_to("C_OTHER", "hello", "allan"));
        assert!(coordinator.addressed_to("C123", "<@UBOT1> hello", "allan"));
        assert!(!coordinator.addressed_to("C123", "<@UBOT2> hello", "allan"));
        assert!(!coordinator.addressed_to("C123", "hello all", "allan"));
    }

    fn config_with_default() -> SharedChannelConfig {
        SharedChannelConfig {
            channel_id: "C456".to_string(),
            agents: vec![
                ChannelAgentId {
                    agent_id: "main-bot".to_string(),
                    bot_user_id: "UDEFAULT".to_string(),
                    display_name: Some("MainBot".to_string()),
                },
                ChannelAgentId {
                    agent_id: "specialist".to_string(),
                    bot_user_id: "USPEC".to_string(),
                    display_name: Some("Specialist".to_string()),
                },
            ],
            default_agent: Some("main-bot".to_string()),
            claim_timeout_ms: 500,
        }
    }

    #[test]
    fn uncoordinated_channel_returns_uncoordinated() {
        let coord = ChannelCoordinator::new(vec![test_config()]);
        // Channel "C999" is not registered
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(coord.try_claim("C999", "1111.1111", "allan", "hello"));
        assert_eq!(result, ClaimResult::Uncoordinated);
    }

    #[test]
    fn direct_mention_routes_to_correct_agent() {
        let coord = ChannelCoordinator::new(vec![test_config()]);
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Message mentions reviewer
        let result = rt.block_on(coord.try_claim(
            "C123",
            "1111.1111",
            "reviewer",
            "hey <@UBOT2> review this PR",
        ));
        assert_eq!(result, ClaimResult::Claimed);

        // Allan tries to claim the same message — should see it's for reviewer
        let result = rt.block_on(coord.try_claim(
            "C123",
            "1111.1111",
            "allan",
            "hey <@UBOT2> review this PR",
        ));
        assert_eq!(result, ClaimResult::MentionedOther("reviewer".to_string()));
    }

    #[test]
    fn first_agent_claims_unmentioned_message() {
        let coord = ChannelCoordinator::new(vec![test_config()]);
        let rt = tokio::runtime::Runtime::new().unwrap();

        // No one mentioned — first to claim wins
        let result = rt.block_on(coord.try_claim("C123", "2222.2222", "allan", "hello team"));
        assert_eq!(result, ClaimResult::Claimed);

        // Reviewer tries to claim the same message
        let result = rt.block_on(coord.try_claim("C123", "2222.2222", "reviewer", "hello team"));
        assert_eq!(result, ClaimResult::AlreadyClaimed("allan".to_string()));
    }

    #[test]
    fn default_agent_gets_priority() {
        let coord = ChannelCoordinator::new(vec![config_with_default()]);
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Specialist tries to claim an unmentioned message — defers to default
        let result =
            rt.block_on(coord.try_claim("C456", "3333.3333", "specialist", "general question"));
        assert_eq!(result, ClaimResult::AlreadyClaimed("main-bot".to_string()));

        // Default agent claims it
        let result =
            rt.block_on(coord.try_claim("C456", "3333.3333", "main-bot", "general question"));
        assert_eq!(result, ClaimResult::Claimed);
    }

    #[test]
    fn mention_overrides_default_agent() {
        let coord = ChannelCoordinator::new(vec![config_with_default()]);
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Specialist is mentioned directly — overrides default
        let result = rt.block_on(coord.try_claim(
            "C456",
            "4444.4444",
            "specialist",
            "<@USPEC> help me with this",
        ));
        assert_eq!(result, ClaimResult::Claimed);

        // Default tries to claim — sees specialist was mentioned
        let result = rt.block_on(coord.try_claim(
            "C456",
            "4444.4444",
            "main-bot",
            "<@USPEC> help me with this",
        ));
        assert_eq!(
            result,
            ClaimResult::MentionedOther("specialist".to_string())
        );
    }

    #[test]
    fn same_agent_can_reclaim() {
        let coord = ChannelCoordinator::new(vec![test_config()]);
        let rt = tokio::runtime::Runtime::new().unwrap();

        let result = rt.block_on(coord.try_claim("C123", "5555.5555", "allan", "hello"));
        assert_eq!(result, ClaimResult::Claimed);

        // Same agent, same message — still claimed
        let result = rt.block_on(coord.try_claim("C123", "5555.5555", "allan", "hello"));
        assert_eq!(result, ClaimResult::Claimed);
    }

    #[test]
    fn mentioned_agent_detection() {
        let coord = ChannelCoordinator::new(vec![test_config()]);

        assert_eq!(
            coord.mentioned_agent("C123", "hey <@UBOT1> do this"),
            Some("allan".to_string())
        );
        assert_eq!(
            coord.mentioned_agent("C123", "<@UBOT3> help"),
            Some("helper".to_string())
        );
        assert_eq!(coord.mentioned_agent("C123", "hello everyone"), None);
        // Multiple mentions — ambiguous, returns None
        assert_eq!(coord.mentioned_agent("C123", "<@UBOT1> and <@UBOT2>"), None);
        // Unknown channel
        assert_eq!(coord.mentioned_agent("C999", "<@UBOT1>"), None);
    }

    #[test]
    fn display_name_mention_detection() {
        let coord = ChannelCoordinator::new(vec![test_config()]);

        // Plain-text @DisplayName (case-insensitive)
        assert_eq!(
            coord.mentioned_agent("C123", "hey @Allan can you help?"),
            Some("allan".to_string())
        );
        assert_eq!(
            coord.mentioned_agent("C123", "@REVIEWER please check this"),
            Some("reviewer".to_string())
        );
        // Case insensitive
        assert_eq!(
            coord.mentioned_agent("C123", "ask @helper for support"),
            Some("helper".to_string())
        );
        // No match
        assert_eq!(coord.mentioned_agent("C123", "hello @nobody"), None);
        // Multiple display name mentions — ambiguous
        assert_eq!(coord.mentioned_agent("C123", "@Allan and @Reviewer"), None);
    }

    #[test]
    fn display_name_overrides_default_agent() {
        let coord = ChannelCoordinator::new(vec![config_with_default()]);
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Specialist mentioned by display name — overrides default
        let result = rt.block_on(coord.try_claim(
            "C456",
            "9999.9999",
            "specialist",
            "hey @Specialist help me",
        ));
        assert_eq!(result, ClaimResult::Claimed);

        // Default tries same message — sees specialist mentioned
        let result = rt.block_on(coord.try_claim(
            "C456",
            "9999.9999",
            "main-bot",
            "hey @Specialist help me",
        ));
        assert_eq!(
            result,
            ClaimResult::MentionedOther("specialist".to_string())
        );
    }

    #[test]
    fn other_agents_listing() {
        let coord = ChannelCoordinator::new(vec![test_config()]);
        let others = coord.other_agents("C123", "allan");
        assert_eq!(others.len(), 2);
        assert!(others.iter().any(|a| a.agent_id == "reviewer"));
        assert!(others.iter().any(|a| a.agent_id == "helper"));

        // Unknown channel
        let others = coord.other_agents("C999", "allan");
        assert!(others.is_empty());
    }

    #[test]
    fn different_messages_can_be_claimed_by_different_agents() {
        let coord = ChannelCoordinator::new(vec![test_config()]);
        let rt = tokio::runtime::Runtime::new().unwrap();

        let r1 = rt.block_on(coord.try_claim("C123", "6666.6666", "allan", "first msg"));
        assert_eq!(r1, ClaimResult::Claimed);

        let r2 = rt.block_on(coord.try_claim("C123", "7777.7777", "reviewer", "second msg"));
        assert_eq!(r2, ClaimResult::Claimed);

        // But cross-claims fail
        let r3 = rt.block_on(coord.try_claim("C123", "6666.6666", "reviewer", "first msg"));
        assert_eq!(r3, ClaimResult::AlreadyClaimed("allan".to_string()));
    }

    #[test]
    fn record_response_works() {
        let coord = ChannelCoordinator::new(vec![test_config()]);
        let rt = tokio::runtime::Runtime::new().unwrap();

        rt.block_on(coord.record_response("C123", "8888.8888", "allan"));

        // Now another agent can't claim it
        let result = rt.block_on(coord.try_claim("C123", "8888.8888", "reviewer", "some text"));
        assert_eq!(result, ClaimResult::AlreadyClaimed("allan".to_string()));
    }

    #[test]
    fn is_coordinated_check() {
        let coord = ChannelCoordinator::new(vec![test_config()]);
        assert!(coord.is_coordinated("C123"));
        assert!(!coord.is_coordinated("C999"));
    }

    #[test]
    fn empty_coordinator() {
        let coord = ChannelCoordinator::empty();
        assert!(!coord.is_coordinated("C123"));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(coord.try_claim("C123", "1111.1111", "allan", "hello"));
        assert_eq!(result, ClaimResult::Uncoordinated);
    }
}
