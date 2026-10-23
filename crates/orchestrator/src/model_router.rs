//! Model routing — intelligent LLM model selection based on task characteristics.
//!
//! Agents have default models, but the orchestrator can override based on
//! complexity, budget, and task type.

use serde::{Deserialize, Serialize};

/// Model tier for cost/capability classification.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    /// Fast, cheap — formatting, classification, simple lookups.
    Economy,
    /// Balanced — most development, code review, testing.
    Standard,
    /// Best capability — architecture, complex debugging, security.
    Premium,
}

/// A model routing rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    /// Rule name for identification.
    pub name: String,
    /// Conditions that trigger this rule.
    pub condition: RoutingCondition,
    /// Target model or tier.
    pub target: RoutingTarget,
    /// Priority (higher = checked first).
    #[serde(default)]
    pub priority: i32,
}

/// Conditions for model routing decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoutingCondition {
    /// Task type matches.
    TaskType { types: Vec<String> },
    /// Task has certain labels.
    Labels { labels: Vec<String> },
    /// Task complexity level.
    Complexity { min: TaskComplexity },
    /// Budget remaining is below threshold (0.0 to 1.0).
    BudgetLow { threshold: f32 },
    /// Agent has a stored model preference for this task type.
    AgentPreference,
    /// Always matches.
    Always,
}

/// Task complexity classification.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskComplexity {
    Simple,
    Medium,
    High,
    Critical,
}

/// What model to route to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoutingTarget {
    /// Use a specific model.
    Model { model: String },
    /// Use the best available in a tier.
    Tier { tier: ModelTier },
    /// Downgrade from agent's default.
    Downgrade,
    /// Upgrade from agent's default.
    Upgrade,
    /// Use the agent's self-selected model preference.
    AgentPreference,
}

/// Context for making a routing decision.
#[derive(Debug, Clone, Default)]
pub struct RoutingContext {
    pub task_type: Option<String>,
    pub labels: Vec<String>,
    pub complexity: Option<TaskComplexity>,
    pub budget_remaining_ratio: f32,
    pub agent_default_model: Option<String>,
    /// Agent's self-selected model preference (from telemetry/optimization).
    pub agent_preference: Option<String>,
}

/// Known model definitions for tier mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDefinition {
    pub name: String,
    pub tier: ModelTier,
    /// Cost per 1K input tokens (USD).
    pub cost_per_1k_input: f64,
    /// Cost per 1K output tokens (USD).
    pub cost_per_1k_output: f64,
}

/// The model router — selects the best model for a task.
pub struct ModelRouter {
    rules: Vec<RoutingRule>,
    models: Vec<ModelDefinition>,
}

impl ModelRouter {
    pub fn new(rules: Vec<RoutingRule>, models: Vec<ModelDefinition>) -> Self {
        let mut rules = rules;
        rules.sort_by(|a, b| b.priority.cmp(&a.priority));
        Self { rules, models }
    }

    /// Create with default model definitions.
    pub fn with_defaults(rules: Vec<RoutingRule>) -> Self {
        Self::new(rules, default_models())
    }

    /// Select a model based on context and routing rules.
    pub fn select(&self, context: &RoutingContext) -> Option<String> {
        for rule in &self.rules {
            if self.matches_condition(&rule.condition, context) {
                return self.resolve_target(&rule.target, context);
            }
        }
        None // Use agent's default
    }

    /// Get the cost estimate for a model (per 1K tokens).
    pub fn model_cost(&self, model: &str) -> Option<(f64, f64)> {
        self.models
            .iter()
            .find(|m| m.name == model)
            .map(|m| (m.cost_per_1k_input, m.cost_per_1k_output))
    }

    /// Get the tier for a model.
    pub fn model_tier(&self, model: &str) -> Option<ModelTier> {
        self.models
            .iter()
            .find(|m| m.name == model)
            .map(|m| m.tier.clone())
    }

    /// List models in a tier.
    pub fn models_in_tier(&self, tier: &ModelTier) -> Vec<&ModelDefinition> {
        self.models.iter().filter(|m| m.tier == *tier).collect()
    }

    fn matches_condition(&self, condition: &RoutingCondition, context: &RoutingContext) -> bool {
        match condition {
            RoutingCondition::TaskType { types } => context
                .task_type
                .as_ref()
                .is_some_and(|t| types.contains(t)),
            RoutingCondition::Labels { labels } => {
                labels.iter().any(|l| context.labels.contains(l))
            }
            RoutingCondition::Complexity { min } => {
                context.complexity.as_ref().is_some_and(|c| c >= min)
            }
            RoutingCondition::BudgetLow { threshold } => {
                context.budget_remaining_ratio < *threshold
            }
            RoutingCondition::AgentPreference => context.agent_preference.is_some(),
            RoutingCondition::Always => true,
        }
    }

    fn resolve_target(&self, target: &RoutingTarget, context: &RoutingContext) -> Option<String> {
        match target {
            RoutingTarget::Model { model } => Some(model.clone()),
            RoutingTarget::Tier { tier } => {
                // Pick the cheapest model in the tier
                self.models_in_tier(tier)
                    .into_iter()
                    .min_by(|a, b| {
                        a.cost_per_1k_input
                            .partial_cmp(&b.cost_per_1k_input)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|m| m.name.clone())
            }
            RoutingTarget::Downgrade => {
                let current_tier = context
                    .agent_default_model
                    .as_ref()
                    .and_then(|m| self.model_tier(m))
                    .unwrap_or(ModelTier::Standard);

                let lower_tier = match current_tier {
                    ModelTier::Premium => ModelTier::Standard,
                    ModelTier::Standard => ModelTier::Economy,
                    ModelTier::Economy => ModelTier::Economy,
                };

                self.resolve_target(&RoutingTarget::Tier { tier: lower_tier }, context)
            }
            RoutingTarget::Upgrade => {
                let current_tier = context
                    .agent_default_model
                    .as_ref()
                    .and_then(|m| self.model_tier(m))
                    .unwrap_or(ModelTier::Standard);

                let higher_tier = match current_tier {
                    ModelTier::Economy => ModelTier::Standard,
                    ModelTier::Standard => ModelTier::Premium,
                    ModelTier::Premium => ModelTier::Premium,
                };

                self.resolve_target(&RoutingTarget::Tier { tier: higher_tier }, context)
            }
            RoutingTarget::AgentPreference => context.agent_preference.clone(),
        }
    }
}

fn default_models() -> Vec<ModelDefinition> {
    vec![
        ModelDefinition {
            name: "claude-haiku-4".to_string(),
            tier: ModelTier::Economy,
            cost_per_1k_input: 0.0008,
            cost_per_1k_output: 0.004,
        },
        ModelDefinition {
            name: "claude-sonnet-4".to_string(),
            tier: ModelTier::Standard,
            cost_per_1k_input: 0.003,
            cost_per_1k_output: 0.015,
        },
        ModelDefinition {
            name: "claude-opus-4".to_string(),
            tier: ModelTier::Premium,
            cost_per_1k_input: 0.015,
            cost_per_1k_output: 0.075,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rules() -> Vec<RoutingRule> {
        vec![
            RoutingRule {
                name: "architecture-needs-opus".to_string(),
                condition: RoutingCondition::TaskType {
                    types: vec!["architecture".to_string(), "security-review".to_string()],
                },
                target: RoutingTarget::Tier {
                    tier: ModelTier::Premium,
                },
                priority: 10,
            },
            RoutingRule {
                name: "simple-tasks-use-haiku".to_string(),
                condition: RoutingCondition::Complexity {
                    min: TaskComplexity::Simple,
                },
                target: RoutingTarget::Tier {
                    tier: ModelTier::Economy,
                },
                priority: 1,
            },
            RoutingRule {
                name: "low-budget-downgrade".to_string(),
                condition: RoutingCondition::BudgetLow { threshold: 0.2 },
                target: RoutingTarget::Downgrade,
                priority: 20,
            },
        ]
    }

    #[test]
    fn route_by_task_type() {
        let router = ModelRouter::with_defaults(test_rules());
        let context = RoutingContext {
            task_type: Some("architecture".to_string()),
            budget_remaining_ratio: 0.8,
            ..Default::default()
        };

        let model = router.select(&context).unwrap();
        assert_eq!(model, "claude-opus-4");
    }

    #[test]
    fn route_by_budget() {
        let router = ModelRouter::with_defaults(test_rules());
        let context = RoutingContext {
            budget_remaining_ratio: 0.1, // Very low budget
            agent_default_model: Some("claude-sonnet-4".to_string()),
            ..Default::default()
        };

        let model = router.select(&context).unwrap();
        assert_eq!(model, "claude-haiku-4"); // Downgraded from sonnet to haiku
    }

    #[test]
    fn no_match_returns_none() {
        let router = ModelRouter::with_defaults(vec![]);
        let context = RoutingContext::default();

        assert!(router.select(&context).is_none());
    }

    #[test]
    fn higher_priority_wins() {
        let router = ModelRouter::with_defaults(test_rules());
        // Both "low budget" (priority 20) and "architecture" (priority 10) match
        let context = RoutingContext {
            task_type: Some("architecture".to_string()),
            budget_remaining_ratio: 0.1,
            agent_default_model: Some("claude-sonnet-4".to_string()),
            ..Default::default()
        };

        let model = router.select(&context).unwrap();
        // Budget rule has higher priority
        assert_eq!(model, "claude-haiku-4");
    }

    #[test]
    fn model_cost_lookup() {
        let router = ModelRouter::with_defaults(vec![]);
        let (input, output) = router.model_cost("claude-sonnet-4").unwrap();
        assert_eq!(input, 0.003);
        assert_eq!(output, 0.015);
    }

    #[test]
    fn models_in_tier() {
        let router = ModelRouter::with_defaults(vec![]);
        let economy = router.models_in_tier(&ModelTier::Economy);
        assert_eq!(economy.len(), 1);
        assert_eq!(economy[0].name, "claude-haiku-4");
    }

    #[test]
    fn upgrade_target() {
        let router = ModelRouter::with_defaults(vec![RoutingRule {
            name: "upgrade".to_string(),
            condition: RoutingCondition::Always,
            target: RoutingTarget::Upgrade,
            priority: 0,
        }]);

        let context = RoutingContext {
            agent_default_model: Some("claude-sonnet-4".to_string()),
            ..Default::default()
        };

        let model = router.select(&context).unwrap();
        assert_eq!(model, "claude-opus-4");
    }

    #[test]
    fn agent_preference_routing() {
        let rules = vec![
            RoutingRule {
                name: "budget-guard".to_string(),
                condition: RoutingCondition::BudgetLow { threshold: 0.2 },
                target: RoutingTarget::Downgrade,
                priority: 20,
            },
            RoutingRule {
                name: "agent-pref".to_string(),
                condition: RoutingCondition::AgentPreference,
                target: RoutingTarget::AgentPreference,
                priority: 5,
            },
        ];
        let router = ModelRouter::with_defaults(rules);

        // Agent preference used when budget is fine
        let context = RoutingContext {
            agent_preference: Some("claude-opus-4".to_string()),
            budget_remaining_ratio: 0.8,
            agent_default_model: Some("claude-sonnet-4".to_string()),
            ..Default::default()
        };
        assert_eq!(router.select(&context).unwrap(), "claude-opus-4");

        // Budget guard overrides agent preference
        let context = RoutingContext {
            agent_preference: Some("claude-opus-4".to_string()),
            budget_remaining_ratio: 0.1,
            agent_default_model: Some("claude-sonnet-4".to_string()),
            ..Default::default()
        };
        assert_eq!(router.select(&context).unwrap(), "claude-haiku-4");

        // No preference — falls through to None
        let context = RoutingContext {
            budget_remaining_ratio: 0.8,
            ..Default::default()
        };
        assert!(router.select(&context).is_none());
    }
}
