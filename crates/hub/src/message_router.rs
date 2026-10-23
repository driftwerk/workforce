use serde::{Deserialize, Serialize};
use tracing::debug;

/// An agent's skill profile for routing purposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSkillProfile {
    /// Agent identifier.
    pub agent_id: String,
    /// Skills/domains this agent handles.
    pub domains: Vec<String>,
    /// Keywords that trigger routing to this agent.
    pub keywords: Vec<String>,
    /// Description of what this agent does (used for display).
    pub description: String,
    /// Priority weight (higher = preferred when scores are equal).
    #[serde(default)]
    pub priority: u32,
}

/// Result of routing a message to an agent.
#[derive(Debug, Clone)]
pub struct RouteResult {
    /// The agent to route to.
    pub agent_id: String,
    /// Confidence score (0.0 to 1.0).
    pub score: f64,
    /// Which keywords/domains matched.
    pub matched: Vec<String>,
}

/// Routes messages to the most appropriate agent based on skill matching.
///
/// Uses keyword extraction and domain matching for fast, LLM-free routing.
/// Falls back to a default agent when no strong match is found.
pub struct MessageRouter {
    /// Agent skill profiles.
    profiles: Vec<AgentSkillProfile>,
    /// Default agent for unroutable messages.
    default_agent: Option<String>,
    /// Minimum score threshold to consider a match.
    min_score: f64,
}

impl MessageRouter {
    /// Create a new router with agent profiles.
    pub fn new(profiles: Vec<AgentSkillProfile>, default_agent: Option<String>) -> Self {
        Self {
            profiles,
            default_agent,
            min_score: 0.1,
        }
    }

    /// Create an empty router (no routing, always returns default or None).
    pub fn empty() -> Self {
        Self {
            profiles: Vec::new(),
            default_agent: None,
            min_score: 0.1,
        }
    }

    /// Route a message to the best-matching agent.
    pub fn route(&self, message: &str) -> Option<RouteResult> {
        if self.profiles.is_empty() {
            return self.default_agent.as_ref().map(|id| RouteResult {
                agent_id: id.clone(),
                score: 0.0,
                matched: vec!["default".to_string()],
            });
        }

        let message_lower = message.to_lowercase();
        let words: Vec<&str> = message_lower.split_whitespace().collect();

        let mut scores: Vec<(usize, f64, Vec<String>)> = Vec::new();

        for (idx, profile) in self.profiles.iter().enumerate() {
            let (score, matched) = self.score_profile(profile, &message_lower, &words);
            if score > 0.0 {
                scores.push((idx, score, matched));
            }
        }

        // Sort by score descending, then priority descending
        scores.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    self.profiles[b.0]
                        .priority
                        .cmp(&self.profiles[a.0].priority)
                })
        });

        if let Some((idx, score, matched)) = scores.into_iter().next() {
            if score >= self.min_score {
                return Some(RouteResult {
                    agent_id: self.profiles[idx].agent_id.clone(),
                    score,
                    matched,
                });
            }
        }

        // Fall back to default
        self.default_agent.as_ref().map(|id| RouteResult {
            agent_id: id.clone(),
            score: 0.0,
            matched: vec!["default_fallback".to_string()],
        })
    }

    /// Score a message against an agent's profile.
    fn score_profile(
        &self,
        profile: &AgentSkillProfile,
        message_lower: &str,
        _words: &[&str],
    ) -> (f64, Vec<String>) {
        let mut score = 0.0;
        let mut matched = Vec::new();
        let total_possible = (profile.keywords.len() + profile.domains.len()).max(1) as f64;

        // Check keyword matches
        for keyword in &profile.keywords {
            let kw_lower = keyword.to_lowercase();
            if message_lower.contains(&kw_lower) {
                score += 1.0;
                matched.push(format!("keyword:{}", keyword));
            }
        }

        // Check domain matches (broader — check if any word relates to domain)
        for domain in &profile.domains {
            let domain_lower = domain.to_lowercase();
            // Direct domain mention
            if message_lower.contains(&domain_lower) {
                score += 0.5;
                matched.push(format!("domain:{}", domain));
            }
        }

        // Normalize score
        let normalized = score / total_possible;

        debug!(
            agent = %profile.agent_id,
            score = normalized,
            matches = matched.len(),
            "Routing score"
        );

        (normalized, matched)
    }

    /// Get all registered agent profiles.
    pub fn profiles(&self) -> &[AgentSkillProfile] {
        &self.profiles
    }
}

/// Configuration for message routing in shared channels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// Agent skill profiles.
    pub agents: Vec<AgentSkillProfile>,
    /// Default agent for unroutable messages.
    #[serde(default)]
    pub default_agent: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_profiles() -> Vec<AgentSkillProfile> {
        vec![
            AgentSkillProfile {
                agent_id: "code-reviewer".to_string(),
                domains: vec!["code review".to_string(), "programming".to_string()],
                keywords: vec![
                    "PR".to_string(),
                    "pull request".to_string(),
                    "code review".to_string(),
                    "merge".to_string(),
                    "refactor".to_string(),
                ],
                description: "Reviews code and PRs".to_string(),
                priority: 0,
            },
            AgentSkillProfile {
                agent_id: "devops".to_string(),
                domains: vec![
                    "deployment".to_string(),
                    "infrastructure".to_string(),
                    "monitoring".to_string(),
                ],
                keywords: vec![
                    "deploy".to_string(),
                    "server".to_string(),
                    "docker".to_string(),
                    "kubernetes".to_string(),
                    "CI/CD".to_string(),
                    "monitoring".to_string(),
                    "uptime".to_string(),
                ],
                description: "Handles deployment and infrastructure".to_string(),
                priority: 0,
            },
            AgentSkillProfile {
                agent_id: "general".to_string(),
                domains: vec!["general".to_string()],
                keywords: vec![],
                description: "General assistant".to_string(),
                priority: 0,
            },
        ]
    }

    #[test]
    fn routes_to_code_reviewer() {
        let router = MessageRouter::new(test_profiles(), Some("general".to_string()));
        let result = router.route("Can you review this PR?").unwrap();
        assert_eq!(result.agent_id, "code-reviewer");
        assert!(result.score > 0.0);
    }

    #[test]
    fn routes_to_devops() {
        let router = MessageRouter::new(test_profiles(), Some("general".to_string()));
        let result = router
            .route("We need to deploy the new version to the server")
            .unwrap();
        assert_eq!(result.agent_id, "devops");
    }

    #[test]
    fn falls_back_to_default() {
        let router = MessageRouter::new(test_profiles(), Some("general".to_string()));
        let result = router.route("What's the weather like?").unwrap();
        assert_eq!(result.agent_id, "general");
        assert_eq!(result.score, 0.0);
    }

    #[test]
    fn no_profiles_returns_default() {
        let router = MessageRouter::new(vec![], Some("fallback".to_string()));
        let result = router.route("anything").unwrap();
        assert_eq!(result.agent_id, "fallback");
    }

    #[test]
    fn no_profiles_no_default_returns_none() {
        let router = MessageRouter::empty();
        assert!(router.route("anything").is_none());
    }

    #[test]
    fn case_insensitive_matching() {
        let router = MessageRouter::new(test_profiles(), None);
        let result = router.route("please review this PULL REQUEST").unwrap();
        assert_eq!(result.agent_id, "code-reviewer");
    }

    #[test]
    fn multiple_keyword_matches_score_higher() {
        let router = MessageRouter::new(test_profiles(), None);
        // "deploy" and "server" and "docker" — strong devops match
        let result = router
            .route("deploy the docker container to the server")
            .unwrap();
        assert_eq!(result.agent_id, "devops");
        assert!(result.score > 0.2);
    }

    #[test]
    fn priority_breaks_ties() {
        let profiles = vec![
            AgentSkillProfile {
                agent_id: "agent-a".to_string(),
                domains: vec![],
                keywords: vec!["help".to_string()],
                description: "A".to_string(),
                priority: 1,
            },
            AgentSkillProfile {
                agent_id: "agent-b".to_string(),
                domains: vec![],
                keywords: vec!["help".to_string()],
                description: "B".to_string(),
                priority: 10,
            },
        ];
        let router = MessageRouter::new(profiles, None);
        let result = router.route("help me").unwrap();
        assert_eq!(result.agent_id, "agent-b");
    }

    #[test]
    fn domain_matching() {
        let router = MessageRouter::new(test_profiles(), None);
        let result = router.route("I have a code review question").unwrap();
        assert_eq!(result.agent_id, "code-reviewer");
        assert!(result.matched.iter().any(|m| m.starts_with("keyword:")));
    }

    #[test]
    fn matched_list_populated() {
        let router = MessageRouter::new(test_profiles(), None);
        let result = router.route("review this PR and merge it").unwrap();
        assert_eq!(result.agent_id, "code-reviewer");
        assert!(result.matched.len() >= 2); // PR + merge at minimum
    }
}
