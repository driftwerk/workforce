//! A person's own route: everything billable to them run on their own
//! subscription instead of the hub's model route. The hub's switch
//! (`[llm.personal_routes]`), an agent's opt-out from it, and the person's
//! two settings (every agent, and one agent), the narrower winning. Pure
//! types; the store and the routing live in the hub.

use serde::{Deserialize, Serialize};

use crate::subscription::SubscriptionKind;

/// The subscriptions a person may run their work on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PersonalRouteKind {
    ClaudeCode,
    Codex,
    Devin,
}

impl PersonalRouteKind {
    pub const ALL: [Self; 3] = [Self::ClaudeCode, Self::Codex, Self::Devin];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Devin => "devin",
        }
    }

    /// How the route reads to a person: "My Claude".
    pub const fn title(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude",
            Self::Codex => "ChatGPT (Codex)",
            Self::Devin => "Devin",
        }
    }

    /// The subscription login this route runs on, when the hub keeps logins
    /// of its kind.
    pub const fn login_kind(self) -> Option<SubscriptionKind> {
        match self {
            Self::ClaudeCode => Some(SubscriptionKind::Claude),
            Self::Codex => Some(SubscriptionKind::ChatGpt),
            Self::Devin => None,
        }
    }
}

impl std::fmt::Display for PersonalRouteKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a call on a person's route does when their login is missing or
/// their plan is used up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteFallback {
    /// The call stops and says why, and when the plan is back.
    #[default]
    Wait,
    /// That one call runs on the hub's route instead, and says so.
    Hub,
}

impl RouteFallback {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Hub => "hub",
        }
    }
}

/// A person's own route: the subscription their work runs on, and what
/// happens when it can't.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonalRoute {
    pub kind: PersonalRouteKind,
    #[serde(default)]
    pub fallback: RouteFallback,
}

/// A person's setting for one agent: their own route there, or the hub's
/// route whatever their every-agent setting says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentRoute {
    Hub,
    Personal(PersonalRoute),
}

const HUB_WORD: &str = "hub";

impl Serialize for AgentRoute {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Hub => serializer.serialize_str(HUB_WORD),
            Self::Personal(route) => route.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for AgentRoute {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Word(String),
            Route(PersonalRoute),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Word(word) if word == HUB_WORD => Ok(Self::Hub),
            Repr::Word(word) => Err(serde::de::Error::custom(format!(
                "expected \"{HUB_WORD}\" or {{kind, fallback}}, got \"{word}\""
            ))),
            Repr::Route(route) => Ok(Self::Personal(route)),
        }
    }
}

/// The route a person's call on one agent takes, from their two settings:
/// the agent's own when there is one, else the every-agent one. `None` is
/// the hub's route.
pub fn chosen_route(
    every_agent: Option<PersonalRoute>,
    this_agent: Option<AgentRoute>,
) -> Option<PersonalRoute> {
    match this_agent {
        Some(AgentRoute::Hub) => None,
        Some(AgentRoute::Personal(route)) => Some(route),
        None => every_agent,
    }
}

/// The hub's switch, `[llm.personal_routes]`: whether people may run their
/// work on their own subscriptions here, and on which. Off by default; read
/// live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonalRoutesConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_allow")]
    pub allow: Vec<PersonalRouteKind>,
}

fn default_allow() -> Vec<PersonalRouteKind> {
    PersonalRouteKind::ALL.to_vec()
}

impl Default for PersonalRoutesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow: default_allow(),
        }
    }
}

/// An agent's own `[llm.personal_routes]`: it can only narrow the hub's
/// switch, never widen it. `enabled = false` opts the agent out (a
/// customer-facing agent, say); `allow` keeps only the kinds both name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPersonalRoutes {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub allow: Option<Vec<PersonalRouteKind>>,
}

impl PersonalRoutesConfig {
    /// The switch as it stands for one agent: on only when the hub's is on
    /// and the agent hasn't opted out, offering the kinds both allow.
    pub fn for_agent(&self, agent: Option<&AgentPersonalRoutes>) -> Self {
        let opted_out = agent.and_then(|a| a.enabled) == Some(false);
        let allow = self
            .allow
            .iter()
            .copied()
            .filter(|kind| {
                agent
                    .and_then(|a| a.allow.as_ref())
                    .is_none_or(|narrowed| narrowed.contains(kind))
            })
            .collect();
        Self {
            enabled: self.enabled && !opted_out,
            allow,
        }
    }

    /// Whether a person's route of `kind` may run here.
    pub fn admits(&self, kind: PersonalRouteKind) -> bool {
        self.enabled && self.allow.contains(&kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: PersonalRoute = PersonalRoute {
        kind: PersonalRouteKind::ClaudeCode,
        fallback: RouteFallback::Wait,
    };
    const CODEX_HUB: PersonalRoute = PersonalRoute {
        kind: PersonalRouteKind::Codex,
        fallback: RouteFallback::Hub,
    };

    #[test]
    fn the_narrower_setting_wins_and_hub_refuses_the_every_agent_one() {
        assert_eq!(chosen_route(None, None), None);
        assert_eq!(chosen_route(Some(CLAUDE), None), Some(CLAUDE));
        assert_eq!(chosen_route(Some(CLAUDE), Some(AgentRoute::Hub)), None);
        assert_eq!(
            chosen_route(Some(CLAUDE), Some(AgentRoute::Personal(CODEX_HUB))),
            Some(CODEX_HUB)
        );
        assert_eq!(
            chosen_route(None, Some(AgentRoute::Personal(CLAUDE))),
            Some(CLAUDE)
        );
    }

    #[test]
    fn an_agent_route_reads_as_hub_or_a_route() {
        let hub: AgentRoute = serde_json::from_str("\"hub\"").unwrap();
        assert_eq!(hub, AgentRoute::Hub);
        let route: AgentRoute = serde_json::from_str(r#"{"kind":"claude-code"}"#).unwrap();
        assert_eq!(route, AgentRoute::Personal(CLAUDE));
        assert_eq!(serde_json::to_string(&AgentRoute::Hub).unwrap(), "\"hub\"");
        assert_eq!(
            serde_json::to_value(AgentRoute::Personal(CODEX_HUB)).unwrap(),
            serde_json::json!({"kind": "codex", "fallback": "hub"})
        );
        assert!(serde_json::from_str::<AgentRoute>("\"mine\"").is_err());
        assert!(serde_json::from_str::<AgentRoute>(r#"{"kind":"gemini"}"#).is_err());
    }

    #[test]
    fn the_switch_is_off_by_default_and_offers_every_kind() {
        let config: PersonalRoutesConfig = toml::from_str("").unwrap();
        assert_eq!(config, PersonalRoutesConfig::default());
        assert!(!config.enabled);
        assert_eq!(config.allow, PersonalRouteKind::ALL);
        assert!(!config.admits(PersonalRouteKind::ClaudeCode));
        assert!(toml::from_str::<PersonalRoutesConfig>("allow = [\"gemini\"]").is_err());
    }

    #[test]
    fn an_agent_can_only_narrow_the_hubs_switch() {
        let on = PersonalRoutesConfig {
            enabled: true,
            allow: vec![PersonalRouteKind::ClaudeCode, PersonalRouteKind::Codex],
        };
        assert_eq!(on.for_agent(None), on);
        let out = AgentPersonalRoutes {
            enabled: Some(false),
            allow: None,
        };
        assert!(!on.for_agent(Some(&out)).enabled);
        let widen = AgentPersonalRoutes {
            enabled: Some(true),
            allow: Some(PersonalRouteKind::ALL.to_vec()),
        };
        let off = PersonalRoutesConfig::default();
        assert!(
            !off.for_agent(Some(&widen)).enabled,
            "an agent can't turn the hub's switch on"
        );
        assert_eq!(on.for_agent(Some(&widen)).allow, on.allow, "nor add a kind");
        let narrow = AgentPersonalRoutes {
            enabled: None,
            allow: Some(vec![PersonalRouteKind::Codex]),
        };
        let narrowed = on.for_agent(Some(&narrow));
        assert!(narrowed.admits(PersonalRouteKind::Codex));
        assert!(!narrowed.admits(PersonalRouteKind::ClaudeCode));
    }
}
