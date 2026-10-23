//! Slack skill configuration — multi-workspace from the start.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use workforce_common::settings::{ResponseMode, SlackChannelOverride};

fn default_true() -> bool {
    true
}

/// Configuration for a single Slack workspace connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackWorkspaceConfig {
    /// Unique identifier for this workspace (e.g., "acme-corp", "personal").
    pub id: String,
    /// Bot token (xoxb-...). Empty when the token lives in the vault at
    /// `slack.<id>.bot_token`.
    #[serde(default)]
    pub bot_token: String,
    /// App-level token for socket mode (xapp-...). Empty when the token lives
    /// in the vault at `slack.<id>.app_token`.
    #[serde(default)]
    pub app_token: String,
    /// Channels to listen to (empty = all).
    #[serde(default)]
    pub channels: Vec<String>,
    /// Per-workspace response mode override. Falls back to the top-level
    /// `SlackSkillConfig::response_mode` when unset.
    #[serde(default)]
    pub response_mode: Option<ResponseMode>,
    /// Per-workspace thread isolation override. Falls back to the top-level
    /// `SlackSkillConfig::thread_isolation` when unset.
    #[serde(default)]
    pub thread_isolation: Option<bool>,
    /// Per-workspace activation keywords override. Falls back to the top-level
    /// `SlackSkillConfig::activation_keywords` when unset.
    #[serde(default)]
    pub activation_keywords: Option<Vec<String>>,
    /// Per-workspace paragraph spacing override. Falls back to the top-level
    /// `SlackSkillConfig::paragraph_spacing` when unset.
    #[serde(default)]
    pub paragraph_spacing: Option<bool>,
    /// Bot user ID (resolved at runtime via auth.test).
    #[serde(skip)]
    pub bot_user_id: Option<String>,
}

/// Configuration for a slash command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandConfig {
    /// The slash command (e.g., "/ask").
    pub command: String,
    /// Route to a specific agent (optional, uses default if not set).
    #[serde(default)]
    pub agent_id: Option<String>,
    /// System prompt for this command (optional).
    #[serde(default)]
    pub system_prompt: Option<String>,
}

/// Display identity for an agent in Slack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSlackIdentity {
    /// Display name shown in Slack messages.
    pub display_name: String,
    /// Icon URL for the agent's avatar.
    #[serde(default)]
    pub icon_url: Option<String>,
    /// Emoji prefix for messages from this agent (e.g., ":robot:").
    #[serde(default)]
    pub role_emoji: Option<String>,
    /// Role in the team (e.g., "lead", "developer", "reviewer").
    #[serde(default)]
    pub role: Option<String>,
    /// Capabilities for orchestrator scoring (e.g., ["code", "testing"]).
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Areas of expertise for orchestrator scoring (e.g., ["rust", "typescript"]).
    #[serde(default)]
    pub expertise: Vec<String>,
}

/// Configuration for a team channel — multiple agents sharing one Slack channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamChannelConfig {
    /// Team ID this channel belongs to.
    pub team_id: String,
    /// Slack channel ID.
    pub channel_id: String,
    /// Explicit lead agent ID (used for Addressed strategy fallback).
    #[serde(default)]
    pub lead: Option<String>,
    /// Agent identity overrides keyed by agent_id.
    #[serde(default)]
    pub agent_identities: HashMap<String, AgentSlackIdentity>,
    /// Optional workflow to run instead of the ad-hoc delegation chain.
    #[serde(default)]
    pub workflow_id: Option<String>,
}

/// Multi-workspace Slack skill configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackSkillConfig {
    /// Workspace connections. None configured connects nothing.
    #[serde(default)]
    pub workspaces: Vec<SlackWorkspaceConfig>,
    /// Default response mode across all workspaces.
    #[serde(default)]
    pub response_mode: ResponseMode,
    /// Activation keywords — respond when any of these appear in messages.
    #[serde(default)]
    pub activation_keywords: Vec<String>,
    /// Whether to use thread-isolated sessions (default: true).
    #[serde(default = "default_true")]
    pub thread_isolation: bool,
    /// Whether rendered Slack messages get a blank line between paragraphs and
    /// sections, so they read like a human-spaced post (default: true).
    #[serde(default = "default_true")]
    pub paragraph_spacing: bool,
    /// Agent bindings: maps workspace_id to agent_id.
    /// If not specified, the default agent handles all workspaces.
    #[serde(default)]
    pub agent_bindings: HashMap<String, String>,
    /// Per-channel configuration overrides keyed by channel ID.
    #[serde(default)]
    pub channel_overrides: HashMap<String, SlackChannelOverride>,
    /// Slash command configurations.
    #[serde(default)]
    pub slash_commands: Vec<SlashCommandConfig>,
    /// Team channel configurations for multi-agent identity in shared channels.
    #[serde(default)]
    pub team_channels: Vec<TeamChannelConfig>,
}

impl SlackSkillConfig {
    /// Resolve the effective response mode for a given workspace, using the
    /// per-workspace override if set, otherwise falling back to the top-level
    /// `response_mode`.
    pub fn effective_response_mode(&self, workspace_id: &str) -> ResponseMode {
        self.workspaces
            .iter()
            .find(|ws| ws.id == workspace_id)
            .and_then(|ws| ws.response_mode.clone())
            .unwrap_or_else(|| self.response_mode.clone())
    }

    /// Resolve the effective thread isolation flag for a given workspace,
    /// using the per-workspace override if set, otherwise falling back to the
    /// top-level `thread_isolation`.
    pub fn effective_thread_isolation(&self, workspace_id: &str) -> bool {
        self.workspaces
            .iter()
            .find(|ws| ws.id == workspace_id)
            .and_then(|ws| ws.thread_isolation)
            .unwrap_or(self.thread_isolation)
    }

    /// Resolve the effective paragraph spacing for a given workspace, using the
    /// per-workspace override if set, otherwise falling back to the top-level
    /// `paragraph_spacing`.
    pub fn effective_paragraph_spacing(&self, workspace_id: &str) -> bool {
        self.workspaces
            .iter()
            .find(|ws| ws.id == workspace_id)
            .and_then(|ws| ws.paragraph_spacing)
            .unwrap_or(self.paragraph_spacing)
    }

    /// Resolve the effective activation keywords for a given workspace, using
    /// the per-workspace override if set, otherwise falling back to the
    /// top-level `activation_keywords`.
    pub fn effective_activation_keywords(&self, workspace_id: &str) -> Vec<String> {
        self.workspaces
            .iter()
            .find(|ws| ws.id == workspace_id)
            .and_then(|ws| ws.activation_keywords.clone())
            .unwrap_or_else(|| self.activation_keywords.clone())
    }

    /// Generate a default `config.toml` for the Slack skill.
    ///
    /// Used by `workforce skill init slack-integration`.
    pub fn default_toml() -> String {
        // Keep this in sync with the UI-facing template in
        // `workforce-hub::skill_loader::SLACK_DEFAULT_CONFIG_TEMPLATE`.
        r##"# Slack Integration Configuration
#
# Tokens can use env var references: ${SLACK_BOT_TOKEN}

# Top-level defaults — applied to any workspace that does not override them.
# IMPORTANT: these MUST appear before the first `[[workspaces]]` header,
# otherwise TOML scopes them to the workspace block above and they are
# silently ignored by serde.

# Default response mode: "mention_only" | "all" | "quiet"
response_mode = "mention_only"

# Thread isolation: each thread gets its own session context
thread_isolation = true

# Blank line between paragraphs and sections, so messages are easier to read
paragraph_spacing = true

# Activate on these keywords (case-insensitive), even without @mention
# activation_keywords = ["help", "deploy"]

# Agent bindings: map workspace_id to agent_id
# [agent_bindings]
# default = "allan"

[[workspaces]]
id = "default"
bot_token = "${SLACK_BOT_TOKEN}"
app_token = "${SLACK_APP_TOKEN}"
# channels = ["C_GENERAL"]  # empty = all channels
# Per-workspace overrides (take precedence over top-level defaults):
# response_mode = "all"
# thread_isolation = false
# paragraph_spacing = false
# activation_keywords = ["deploy"]

# Per-channel overrides
# [channel_overrides.C0FAKECHAN5]
# system_prompt = "This is #dev — focused on development."
# response_mode = "all"
# display_name = "#dev"

# Per-channel scope filter (optional): classifies inbound messages against a
# topic and short-circuits off-topic ones with a canned response. Presence of
# the [channel_overrides.X.scope] block enables it.
#
# Preset personas: triage | faq | qa | support | sales
# [channel_overrides.C_TRIAGE.scope]
# persona = "triage"
# mode = "reject"  # "reject" (default) | "warn"
# canned_response = "I'm the triage bot. Please file bug reports here."
# model = "claude-haiku-4-5"  # optional classifier model
#
# Or define a custom topic:
# [channel_overrides.C_API_DOCS.scope]
# custom_prompt = """
# On-topic: API usage questions, API bug reports, docs clarifications.
# Off-topic: general chat, unrelated tech questions, personal questions.
# """
"##
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_config_deserializes() {
        let json = r#"{
            "id": "acme",
            "bot_token": "xoxb-test",
            "app_token": "xapp-test",
            "channels": ["C123"]
        }"#;
        let config: SlackWorkspaceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.id, "acme");
        assert_eq!(config.channels, vec!["C123"]);
        assert!(config.bot_user_id.is_none());
    }

    #[test]
    fn workspace_config_empty_channels_default() {
        let json = r#"{
            "id": "acme",
            "bot_token": "xoxb-test",
            "app_token": "xapp-test"
        }"#;
        let config: SlackWorkspaceConfig = serde_json::from_str(json).unwrap();
        assert!(config.channels.is_empty());
    }

    #[test]
    fn skill_config_deserializes() {
        let json = r#"{
            "workspaces": [{
                "id": "default",
                "bot_token": "xoxb-test",
                "app_token": "xapp-test"
            }],
            "response_mode": "mention_only"
        }"#;
        let config: SlackSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.workspaces.len(), 1);
        assert_eq!(config.response_mode, ResponseMode::MentionOnly);
    }

    #[test]
    fn skill_config_response_mode_all() {
        let json = r#"{
            "workspaces": [{
                "id": "default",
                "bot_token": "xoxb-test",
                "app_token": "xapp-test"
            }],
            "response_mode": "all"
        }"#;
        let config: SlackSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.response_mode, ResponseMode::All);
    }

    #[test]
    fn skill_config_defaults() {
        let json = r#"{
            "workspaces": [{
                "id": "default",
                "bot_token": "xoxb-test",
                "app_token": "xapp-test"
            }]
        }"#;
        let config: SlackSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.response_mode, ResponseMode::MentionOnly);
        assert!(config.activation_keywords.is_empty());
        assert!(config.thread_isolation);
        assert!(config.agent_bindings.is_empty());
    }

    #[test]
    fn skill_config_multi_workspace() {
        let json = r#"{
            "workspaces": [
                {
                    "id": "company",
                    "bot_token": "xoxb-company",
                    "app_token": "xapp-company",
                    "channels": ["C_GENERAL"]
                },
                {
                    "id": "client",
                    "bot_token": "xoxb-client",
                    "app_token": "xapp-client"
                }
            ],
            "agent_bindings": {
                "company": "allan",
                "client": "architect"
            }
        }"#;
        let config: SlackSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.workspaces.len(), 2);
        assert_eq!(config.workspaces[0].id, "company");
        assert_eq!(config.workspaces[1].id, "client");
        assert_eq!(config.agent_bindings.get("company").unwrap(), "allan");
        assert_eq!(config.agent_bindings.get("client").unwrap(), "architect");
    }

    #[test]
    fn skill_config_with_channel_overrides() {
        let json = r##"{
            "workspaces": [{
                "id": "default",
                "bot_token": "xoxb-test",
                "app_token": "xapp-test"
            }],
            "channel_overrides": {
                "C_DEV": {
                    "system_prompt": "This is #dev — focused on development.",
                    "response_mode": "all",
                    "display_name": "#dev"
                },
                "C_SUPPORT": {
                    "activation_keywords": ["help", "issue"]
                }
            }
        }"##;
        let config: SlackSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.channel_overrides.len(), 2);

        let dev = config.channel_overrides.get("C_DEV").unwrap();
        assert_eq!(
            dev.system_prompt.as_deref(),
            Some("This is #dev — focused on development.")
        );
        assert_eq!(dev.response_mode, Some(ResponseMode::All));
        assert_eq!(dev.display_name.as_deref(), Some("#dev"));

        let support = config.channel_overrides.get("C_SUPPORT").unwrap();
        assert!(support.system_prompt.is_none());
        assert_eq!(
            support.activation_keywords.as_deref(),
            Some(&["help".to_string(), "issue".to_string()][..])
        );
    }

    #[test]
    fn skill_config_with_scope_filter() {
        use workforce_common::settings::ScopeFilterMode;
        let toml_in = r##"
[[workspaces]]
id = "default"
bot_token = "xoxb-test"
app_token = "xapp-test"

[channel_overrides.C_TRIAGE]
display_name = "#triage"

[channel_overrides.C_TRIAGE.scope]
persona = "triage"
mode = "reject"
canned_response = "triage only please"

[channel_overrides.C_CUSTOM.scope]
custom_prompt = "only api questions"
mode = "warn"
model = "claude-haiku-4-5"

[channel_overrides.C_NO_SCOPE]
display_name = "#chat"
"##;
        let config: SlackSkillConfig = toml::from_str(toml_in).unwrap();

        let triage = config.channel_overrides.get("C_TRIAGE").unwrap();
        let scope = triage.scope.as_ref().expect("scope set");
        assert_eq!(scope.persona.as_deref(), Some("triage"));
        assert_eq!(scope.mode, ScopeFilterMode::Reject);
        assert_eq!(scope.canned_response.as_deref(), Some("triage only please"));
        assert!(scope.custom_prompt.is_none());
        assert!(scope.model.is_none());

        let custom = config.channel_overrides.get("C_CUSTOM").unwrap();
        let scope = custom.scope.as_ref().expect("scope set");
        assert_eq!(scope.custom_prompt.as_deref(), Some("only api questions"));
        assert_eq!(scope.mode, ScopeFilterMode::Warn);
        assert_eq!(scope.model.as_deref(), Some("claude-haiku-4-5"));

        let no_scope = config.channel_overrides.get("C_NO_SCOPE").unwrap();
        assert!(
            no_scope.scope.is_none(),
            "scope absent when no block present"
        );
    }

    #[test]
    fn skill_config_scope_mode_defaults_to_reject() {
        use workforce_common::settings::ScopeFilterMode;
        let toml_in = r##"
[[workspaces]]
id = "default"
bot_token = "xoxb-test"
app_token = "xapp-test"

[channel_overrides.C.scope]
persona = "faq"
"##;
        let config: SlackSkillConfig = toml::from_str(toml_in).unwrap();
        let scope = config
            .channel_overrides
            .get("C")
            .unwrap()
            .scope
            .as_ref()
            .unwrap();
        assert_eq!(scope.mode, ScopeFilterMode::Reject);
    }

    #[test]
    fn skill_config_with_keywords_and_thread_isolation() {
        let json = r#"{
            "workspaces": [{
                "id": "default",
                "bot_token": "xoxb-test",
                "app_token": "xapp-test"
            }],
            "activation_keywords": ["help", "deploy"],
            "thread_isolation": true
        }"#;
        let config: SlackSkillConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.activation_keywords, vec!["help", "deploy"]);
        assert!(config.thread_isolation);
    }

    fn config_with_two_workspaces(
        ws_a_mode: Option<ResponseMode>,
        ws_a_thread: Option<bool>,
        ws_a_keywords: Option<Vec<String>>,
        top_level_mode: ResponseMode,
        top_level_thread: bool,
        top_level_keywords: Vec<String>,
    ) -> SlackSkillConfig {
        SlackSkillConfig {
            workspaces: vec![
                SlackWorkspaceConfig {
                    id: "a".to_string(),
                    bot_token: "xoxb-a".to_string(),
                    app_token: "xapp-a".to_string(),
                    channels: vec![],
                    response_mode: ws_a_mode,
                    thread_isolation: ws_a_thread,
                    activation_keywords: ws_a_keywords,
                    paragraph_spacing: None,
                    bot_user_id: None,
                },
                SlackWorkspaceConfig {
                    id: "b".to_string(),
                    bot_token: "xoxb-b".to_string(),
                    app_token: "xapp-b".to_string(),
                    channels: vec![],
                    response_mode: None,
                    thread_isolation: None,
                    activation_keywords: None,
                    paragraph_spacing: None,
                    bot_user_id: None,
                },
            ],
            response_mode: top_level_mode,
            activation_keywords: top_level_keywords,
            thread_isolation: top_level_thread,
            paragraph_spacing: true,
            agent_bindings: HashMap::new(),
            channel_overrides: HashMap::new(),
            slash_commands: vec![],
            team_channels: vec![],
        }
    }

    #[test]
    fn effective_response_mode_uses_workspace_override_when_set() {
        let config = config_with_two_workspaces(
            Some(ResponseMode::All),
            None,
            None,
            ResponseMode::MentionOnly,
            true,
            vec![],
        );
        assert_eq!(config.effective_response_mode("a"), ResponseMode::All);
        assert_eq!(
            config.effective_response_mode("b"),
            ResponseMode::MentionOnly
        );
    }

    #[test]
    fn effective_response_mode_falls_back_to_top_level() {
        let config =
            config_with_two_workspaces(None, None, None, ResponseMode::Quiet, true, vec![]);
        assert_eq!(config.effective_response_mode("a"), ResponseMode::Quiet);
        assert_eq!(config.effective_response_mode("b"), ResponseMode::Quiet);
    }

    #[test]
    fn effective_response_mode_falls_back_for_unknown_workspace() {
        let config = config_with_two_workspaces(
            Some(ResponseMode::All),
            None,
            None,
            ResponseMode::MentionOnly,
            true,
            vec![],
        );
        assert_eq!(
            config.effective_response_mode("unknown"),
            ResponseMode::MentionOnly
        );
    }

    #[test]
    fn effective_thread_isolation_uses_workspace_override_when_set() {
        let config = config_with_two_workspaces(
            None,
            Some(false),
            None,
            ResponseMode::MentionOnly,
            true,
            vec![],
        );
        assert!(!config.effective_thread_isolation("a"));
        assert!(config.effective_thread_isolation("b"));
    }

    #[test]
    fn effective_thread_isolation_falls_back_to_top_level() {
        let config =
            config_with_two_workspaces(None, None, None, ResponseMode::MentionOnly, false, vec![]);
        assert!(!config.effective_thread_isolation("a"));
        assert!(!config.effective_thread_isolation("b"));
    }

    #[test]
    fn effective_paragraph_spacing_resolves_override_then_default() {
        let mut config =
            config_with_two_workspaces(None, None, None, ResponseMode::MentionOnly, true, vec![]);
        // Top-level default is on; unset workspaces inherit it.
        assert!(config.effective_paragraph_spacing("a"));
        assert!(config.effective_paragraph_spacing("b"));

        config.paragraph_spacing = false;
        config.workspaces[0].paragraph_spacing = Some(true);
        assert!(config.effective_paragraph_spacing("a")); // workspace override wins
        assert!(!config.effective_paragraph_spacing("b")); // top-level default
        assert!(!config.effective_paragraph_spacing("unknown"));
    }

    #[test]
    fn effective_activation_keywords_uses_workspace_override_when_set() {
        let config = config_with_two_workspaces(
            None,
            None,
            Some(vec!["ship".to_string()]),
            ResponseMode::MentionOnly,
            true,
            vec!["help".to_string(), "deploy".to_string()],
        );
        assert_eq!(config.effective_activation_keywords("a"), vec!["ship"]);
        assert_eq!(
            config.effective_activation_keywords("b"),
            vec!["help".to_string(), "deploy".to_string()]
        );
    }

    #[test]
    fn workspace_level_response_mode_deserializes() {
        let json = r#"{
            "workspaces": [{
                "id": "allan",
                "bot_token": "xoxb-a",
                "app_token": "xapp-a",
                "response_mode": "all",
                "thread_isolation": false,
                "activation_keywords": ["ship"]
            }]
        }"#;
        let config: SlackSkillConfig = serde_json::from_str(json).unwrap();
        let ws = &config.workspaces[0];
        assert_eq!(ws.response_mode, Some(ResponseMode::All));
        assert_eq!(ws.thread_isolation, Some(false));
        assert_eq!(
            ws.activation_keywords.as_deref(),
            Some(&["ship".to_string()][..])
        );
        assert_eq!(config.effective_response_mode("allan"), ResponseMode::All);
        assert!(!config.effective_thread_isolation("allan"));
        assert_eq!(config.effective_activation_keywords("allan"), vec!["ship"]);
    }

    #[test]
    fn default_toml_parses_and_has_top_level_defaults() {
        let tpl = SlackSkillConfig::default_toml();
        let config: SlackSkillConfig = toml::from_str(&tpl).expect("default_toml is valid TOML");
        assert_eq!(config.workspaces.len(), 1);
        assert_eq!(config.response_mode, ResponseMode::MentionOnly);
        assert!(config.thread_isolation);
        assert!(config.paragraph_spacing);
    }

    #[test]
    fn slack_workspace_config_silently_ignores_legacy_user_token_field() {
        // Backward compatibility: old config.toml files may still set
        // `user_token = "…"` on a workspace. The field has been removed from
        // `SlackWorkspaceConfig`; serde tolerates unknown fields. The vault
        // value is killed by the startup migration in `SlackManager`.
        let toml_in = r#"
            id = "ws1"
            bot_token = "xoxb-..."
            app_token = "xapp-..."
            user_token = "xoxp-legacy"
        "#;
        let ws: SlackWorkspaceConfig =
            toml::from_str(toml_in).expect("legacy field should parse-and-ignore");
        assert_eq!(ws.id, "ws1");
    }
}
