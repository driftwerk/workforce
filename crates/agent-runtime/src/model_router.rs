use crate::llm::tiers::{ModelTier, TierRegistry};

/// Configuration for adaptive model routing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoutingConfig {
    /// Enable adaptive routing (default: false).
    pub enable_adaptive_routing: bool,
    /// Round at which to consider switching for investigation tasks.
    pub investigation_switch_round: u32,
    /// Round at which to consider switching for coding tasks.
    pub coding_switch_round: u32,
    /// Number of consecutive read-only rounds before switching.
    pub read_only_threshold: u32,
    /// Rounds faster than this (ms) are considered "simple".
    pub fast_round_threshold_ms: u64,
    /// Escalate back to standard model on cheap model failure.
    pub escalate_on_haiku_failure: bool,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            enable_adaptive_routing: false,
            investigation_switch_round: 5,
            coding_switch_round: 10,
            read_only_threshold: 3,
            fast_round_threshold_ms: 30_000, // 30 seconds
            escalate_on_haiku_failure: true,
        }
    }
}

impl From<&workforce_common::settings::WorkspaceRoutingConfig> for RoutingConfig {
    fn from(ws: &workforce_common::settings::WorkspaceRoutingConfig) -> Self {
        let d = Self::default();
        Self {
            enable_adaptive_routing: ws
                .enable_adaptive_routing
                .unwrap_or(d.enable_adaptive_routing),
            investigation_switch_round: ws
                .investigation_switch_round
                .unwrap_or(d.investigation_switch_round),
            coding_switch_round: ws.coding_switch_round.unwrap_or(d.coding_switch_round),
            read_only_threshold: ws.read_only_threshold.unwrap_or(d.read_only_threshold),
            fast_round_threshold_ms: ws
                .fast_round_threshold_ms
                .unwrap_or(d.fast_round_threshold_ms),
            escalate_on_haiku_failure: ws
                .escalate_on_haiku_failure
                .unwrap_or(d.escalate_on_haiku_failure),
        }
    }
}

/// A routing decision returned by the ModelRouter.
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    /// Switch to a cheap-tier model.
    pub switch_to_haiku: bool,
    /// Switch to a standard-tier model.
    pub switch_to_sonnet: bool,
    /// The resolved model to switch to (provider-aware).
    /// `None` when no switch is recommended.
    pub target_model: Option<String>,
    /// Human-readable reason for the decision.
    pub reason: String,
    /// Confidence in this decision (0.0 - 1.0).
    pub confidence: f64,
}

/// Tracks tool usage and complexity for one execution round.
#[derive(Debug, Clone)]
pub struct RoundMetrics {
    pub round_number: u32,
    pub duration_ms: u64,
    pub tools_used: Vec<String>,
    pub has_write_tools: bool,
    pub has_error_recovery: bool,
}

/// The adaptive model router — decides when to switch models mid-task.
///
/// Uses `TierRegistry` for provider-agnostic model classification, so routing
/// decisions work correctly even when running on Gemini or Grok via failover.
pub struct ModelRouter {
    config: RoutingConfig,
    tier_registry: TierRegistry,
    task_type: Option<String>,
    current_model: String,
    round_history: Vec<RoundMetrics>,
    cheap_failure_count: u32,
}

impl ModelRouter {
    pub fn new(config: RoutingConfig, task_type: Option<String>, initial_model: String) -> Self {
        Self::with_tiers(config, TierRegistry::new(None), task_type, initial_model)
    }

    /// Create a router with a custom tier registry.
    pub fn with_tiers(
        config: RoutingConfig,
        tier_registry: TierRegistry,
        task_type: Option<String>,
        initial_model: String,
    ) -> Self {
        Self {
            config,
            tier_registry,
            task_type,
            current_model: initial_model,
            round_history: Vec::new(),
            cheap_failure_count: 0,
        }
    }

    /// Record metrics for a completed round.
    pub fn record_round(&mut self, metrics: RoundMetrics) {
        tracing::debug!(
            target: "routing",
            round = metrics.round_number,
            duration_ms = metrics.duration_ms,
            tools = ?metrics.tools_used,
            write = metrics.has_write_tools,
            error_recovery = metrics.has_error_recovery,
            "Round metrics recorded"
        );
        self.round_history.push(metrics);
    }

    /// Decide whether to switch models based on round history and task type.
    pub fn decide(&self) -> RoutingDecision {
        if !self.config.enable_adaptive_routing {
            return RoutingDecision {
                switch_to_haiku: false,
                switch_to_sonnet: false,
                target_model: None,
                reason: "Adaptive routing disabled".to_string(),
                confidence: 1.0,
            };
        }

        let round = self.round_history.len() as u32 + 1; // Next round number
        let current_tier = self.tier_registry.classify(&self.current_model);
        let current_tier_key = self
            .tier_registry
            .tier_key_for_model(&self.current_model)
            .unwrap_or_else(|| "anthropic".to_string());

        tracing::debug!(
            target: "routing",
            round = round,
            model = %self.current_model,
            tier = ?current_tier,
            task_type = ?self.task_type,
            "Evaluating routing decision"
        );

        // Check if we should downgrade to cheap tier
        if matches!(current_tier, ModelTier::Powerful | ModelTier::Standard) {
            if let Some(task_type) = &self.task_type {
                if (task_type == "conversation" || task_type == "research")
                    && round >= self.config.investigation_switch_round
                    && self.is_repetitive_pattern()
                {
                    let reason = format!(
                        "Investigation task at round {}, repetitive read-only pattern detected",
                        round
                    );
                    let target = self.resolve_tier_model(ModelTier::Cheap, &current_tier_key);
                    tracing::debug!(target: "routing", %reason, ?target, confidence = 0.85, "Recommending switch to cheap tier");
                    return RoutingDecision {
                        switch_to_haiku: true,
                        switch_to_sonnet: false,
                        target_model: target,
                        reason,
                        confidence: 0.85,
                    };
                }

                if task_type == "coding"
                    && round >= self.config.coding_switch_round
                    && !self.has_recent_complex_work()
                {
                    let reason = format!(
                        "Coding task at round {}, no complex work in recent rounds",
                        round
                    );
                    let target = self.resolve_tier_model(ModelTier::Cheap, &current_tier_key);
                    tracing::debug!(target: "routing", %reason, ?target, confidence = 0.75, "Recommending switch to cheap tier");
                    return RoutingDecision {
                        switch_to_haiku: true,
                        switch_to_sonnet: false,
                        target_model: target,
                        reason,
                        confidence: 0.75,
                    };
                }
            }
        }

        // Check if we should escalate from cheap to standard
        if current_tier == ModelTier::Cheap {
            if self.has_error_recovery_pattern() {
                let reason =
                    "Error recovery pattern detected, escalating to standard tier".to_string();
                let target = self.resolve_tier_model(ModelTier::Standard, &current_tier_key);
                tracing::debug!(target: "routing", %reason, ?target, confidence = 0.90, "Recommending switch to standard tier");
                return RoutingDecision {
                    switch_to_haiku: false,
                    switch_to_sonnet: true,
                    target_model: target,
                    reason,
                    confidence: 0.90,
                };
            }

            if self.config.escalate_on_haiku_failure && self.cheap_failure_count >= 3 {
                let reason = format!(
                    "Cheap model failed {} times, escalating to standard tier",
                    self.cheap_failure_count
                );
                let target = self.resolve_tier_model(ModelTier::Standard, &current_tier_key);
                tracing::debug!(target: "routing", %reason, ?target, confidence = 0.95, "Recommending switch to standard tier");
                return RoutingDecision {
                    switch_to_haiku: false,
                    switch_to_sonnet: true,
                    target_model: target,
                    reason,
                    confidence: 0.95,
                };
            }
        }

        // No switch recommended
        tracing::debug!(target: "routing", model = %self.current_model, "No model switch recommended");
        RoutingDecision {
            switch_to_haiku: false,
            switch_to_sonnet: false,
            target_model: None,
            reason: "Current model is optimal".to_string(),
            confidence: 0.80,
        }
    }

    /// Apply a model switch.
    pub fn apply_switch(&mut self, new_model: String) {
        self.current_model = new_model;
        self.cheap_failure_count = 0;
    }

    /// Record a failure (for escalation logic).
    pub fn record_failure(&mut self) {
        if self.tier_registry.classify(&self.current_model) == ModelTier::Cheap {
            self.cheap_failure_count += 1;
        }
    }

    /// Resolve the best model for a target tier, preserving cross-provider routing.
    ///
    /// If the current model has a provider prefix (e.g. `"anthropic:claude-opus-4-6"`),
    /// the resolved model will keep the same prefix so failover routes correctly.
    fn resolve_tier_model(&self, tier: ModelTier, preferred_tier_key: &str) -> Option<String> {
        let (provider_prefix, _) = crate::llm::tiers::split_routing_prefix(&self.current_model);

        let model = self
            .tier_registry
            .resolve_for_tier(tier, preferred_tier_key)
            .or_else(|| {
                ["anthropic", "grok", "gemini"]
                    .iter()
                    .find_map(|key| self.tier_registry.resolve_for_tier(tier, key))
            })?;

        Some(TierRegistry::format_routed(provider_prefix, model))
    }

    /// Detect repetitive read-only tool patterns.
    fn is_repetitive_pattern(&self) -> bool {
        if self.round_history.len() < self.config.read_only_threshold as usize {
            return false;
        }

        let recent = &self.round_history[self
            .round_history
            .len()
            .saturating_sub(self.config.read_only_threshold as usize)..];

        let read_only_tools = [
            "code_read",
            "code_search",
            "filesystem_read",
            "web_search",
            "github_get_file",
            "github_get_pull",
            "linear_issue_get",
            "messaging_read",
        ];

        recent.iter().all(|r| {
            !r.tools_used.is_empty()
                && r.tools_used.iter().all(|tool| {
                    read_only_tools
                        .iter()
                        .any(|read_tool| tool.contains(read_tool))
                })
        })
    }

    /// Check if recent rounds have complex work (write tools, multi-tool, slow).
    fn has_recent_complex_work(&self) -> bool {
        if self.round_history.is_empty() {
            return false;
        }

        let recent_count = 5.min(self.round_history.len());
        let recent = &self.round_history[self.round_history.len() - recent_count..];

        recent.iter().any(|r| {
            r.has_write_tools
                || r.tools_used.len() > 3
                || r.duration_ms > self.config.fast_round_threshold_ms
        })
    }

    /// Check if recent rounds show error recovery patterns.
    fn has_error_recovery_pattern(&self) -> bool {
        if self.round_history.is_empty() {
            return false;
        }

        let recent_count = 3.min(self.round_history.len());
        let recent = &self.round_history[self.round_history.len() - recent_count..];

        recent.iter().any(|r| r.has_error_recovery)
    }

    /// Get current model name.
    pub fn current_model(&self) -> &str {
        &self.current_model
    }

    /// Get round history for telemetry.
    pub fn round_count(&self) -> u32 {
        self.round_history.len() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_config_from_workspace_fills_gaps_with_defaults() {
        let ws = workforce_common::settings::WorkspaceRoutingConfig {
            enable_adaptive_routing: Some(true),
            coding_switch_round: Some(99),
            ..Default::default()
        };
        let cfg = RoutingConfig::from(&ws);
        let d = RoutingConfig::default();
        assert!(cfg.enable_adaptive_routing);
        assert_eq!(cfg.coding_switch_round, 99);
        assert_eq!(cfg.investigation_switch_round, d.investigation_switch_round);
        assert_eq!(cfg.read_only_threshold, d.read_only_threshold);
        assert_eq!(cfg.fast_round_threshold_ms, d.fast_round_threshold_ms);
        assert_eq!(cfg.escalate_on_haiku_failure, d.escalate_on_haiku_failure);
    }

    #[test]
    fn test_routing_disabled() {
        let config = RoutingConfig {
            enable_adaptive_routing: false,
            ..Default::default()
        };
        let router = ModelRouter::new(
            config,
            Some("conversation".to_string()),
            "claude-sonnet-4".to_string(),
        );
        let decision = router.decide();
        assert!(!decision.switch_to_haiku);
        assert!(!decision.switch_to_sonnet);
        assert!(decision.target_model.is_none());
    }

    #[test]
    fn test_investigation_switch_at_round_5() {
        let config = RoutingConfig {
            enable_adaptive_routing: true,
            investigation_switch_round: 5,
            read_only_threshold: 3,
            ..Default::default()
        };
        let mut router = ModelRouter::new(
            config,
            Some("conversation".to_string()),
            "claude-sonnet-4".to_string(),
        );

        for i in 1..=4 {
            router.record_round(RoundMetrics {
                round_number: i,
                duration_ms: 2000,
                tools_used: vec!["code_read".to_string()],
                has_write_tools: false,
                has_error_recovery: false,
            });
        }

        let decision = router.decide();
        assert!(
            decision.switch_to_haiku,
            "Should switch to cheap tier at round 5"
        );
        assert!(
            decision.target_model.is_some(),
            "Should resolve a target model"
        );
    }

    #[test]
    fn test_no_switch_with_complex_work() {
        let config = RoutingConfig {
            enable_adaptive_routing: true,
            investigation_switch_round: 5,
            ..Default::default()
        };
        let mut router = ModelRouter::new(
            config,
            Some("coding".to_string()),
            "claude-sonnet-4".to_string(),
        );

        for i in 1..=10 {
            router.record_round(RoundMetrics {
                round_number: i,
                duration_ms: 5000,
                tools_used: vec!["code_edit".to_string()],
                has_write_tools: true,
                has_error_recovery: false,
            });
        }

        let decision = router.decide();
        assert!(
            !decision.switch_to_haiku,
            "Should NOT switch when doing complex work"
        );
    }

    #[test]
    fn test_escalation_on_error_recovery() {
        let config = RoutingConfig {
            enable_adaptive_routing: true,
            escalate_on_haiku_failure: true,
            ..Default::default()
        };
        let mut router = ModelRouter::new(
            config,
            Some("conversation".to_string()),
            "claude-haiku-4".to_string(),
        );

        router.record_round(RoundMetrics {
            round_number: 1,
            duration_ms: 1000,
            tools_used: vec!["code_read".to_string()],
            has_write_tools: false,
            has_error_recovery: true,
        });

        let decision = router.decide();
        assert!(
            decision.switch_to_sonnet,
            "Should escalate to standard tier on error recovery"
        );
        assert!(decision.target_model.is_some());
    }

    #[test]
    fn test_escalation_after_3_failures() {
        let config = RoutingConfig {
            enable_adaptive_routing: true,
            escalate_on_haiku_failure: true,
            ..Default::default()
        };
        let mut router = ModelRouter::new(
            config,
            Some("conversation".to_string()),
            "claude-haiku-4".to_string(),
        );

        router.record_failure();
        router.record_failure();
        router.record_failure();

        let decision = router.decide();
        assert!(
            decision.switch_to_sonnet,
            "Should escalate to standard tier after 3 failures"
        );
    }

    #[test]
    fn test_escalation_from_bedrock_id_yields_clean_target() {
        // A Bedrock current-model's "-v1:0" colon is not a routing prefix —
        // the 2026-08-18 incident produced hybrid targets like
        // "anthropic.claude-haiku-4-5-20251001-v1:claude-sonnet-5" here.
        let config = RoutingConfig {
            enable_adaptive_routing: true,
            escalate_on_haiku_failure: true,
            ..Default::default()
        };
        let mut router = ModelRouter::new(
            config,
            Some("conversation".to_string()),
            "anthropic.claude-haiku-4-5-20251001-v1:0".to_string(),
        );

        router.record_failure();
        router.record_failure();
        router.record_failure();

        let decision = router.decide();
        assert!(decision.switch_to_sonnet);
        let target = decision.target_model.unwrap();
        assert!(!target.contains(':'), "hybrid routed id leaked: {target}");
    }

    #[test]
    fn test_gemini_model_classification() {
        let config = RoutingConfig {
            enable_adaptive_routing: true,
            investigation_switch_round: 5,
            read_only_threshold: 3,
            ..Default::default()
        };
        let mut router = ModelRouter::new(
            config,
            Some("conversation".to_string()),
            "gemini:gemini-3.1-pro-preview".to_string(),
        );

        // Gemini 2.5 Pro is Powerful tier, should trigger downgrade logic
        for i in 1..=4 {
            router.record_round(RoundMetrics {
                round_number: i,
                duration_ms: 2000,
                tools_used: vec!["code_read".to_string()],
                has_write_tools: false,
                has_error_recovery: false,
            });
        }

        let decision = router.decide();
        assert!(
            decision.switch_to_haiku,
            "Should recommend downgrade for Gemini Powerful model"
        );
        // Target should be a Gemini cheap model
        let target = decision.target_model.unwrap();
        assert!(
            target.contains("gemini"),
            "Target should be a Gemini model, got: {}",
            target
        );
    }

    #[test]
    fn test_grok_model_escalation() {
        let config = RoutingConfig {
            enable_adaptive_routing: true,
            escalate_on_haiku_failure: true,
            ..Default::default()
        };
        // Grok doesn't have a cheap tier model, but let's test with a model
        // that heuristically classifies as Standard
        let mut router = ModelRouter::new(
            config,
            Some("conversation".to_string()),
            "grok:grok-3".to_string(),
        );

        // Standard tier shouldn't trigger cheap escalation, even with failures
        router.record_failure();
        router.record_failure();
        router.record_failure();

        let decision = router.decide();
        // grok-3 is Standard, not Cheap, so escalation shouldn't trigger
        assert!(
            !decision.switch_to_sonnet,
            "Standard tier model should not trigger cheap-model escalation"
        );
    }
}
