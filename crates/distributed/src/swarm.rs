//! Docker Swarm scaling — auto-scale agent worker replicas based on queue depth.
//!
//! Monitors task queue depth and adjusts Swarm service replicas accordingly.

use serde::{Deserialize, Serialize};

/// Swarm scaling configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalingConfig {
    /// Minimum number of worker replicas.
    #[serde(default = "default_min_replicas")]
    pub min_replicas: u32,
    /// Maximum number of worker replicas.
    #[serde(default = "default_max_replicas")]
    pub max_replicas: u32,
    /// Queue depth threshold to scale up (tasks per replica).
    #[serde(default = "default_scale_up_threshold")]
    pub scale_up_threshold: u32,
    /// Queue depth threshold to scale down.
    #[serde(default = "default_scale_down_threshold")]
    pub scale_down_threshold: u32,
    /// Cooldown period between scaling actions (seconds).
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    /// Swarm service name for the agent workers.
    #[serde(default = "default_service_name")]
    pub service_name: String,
}

impl Default for ScalingConfig {
    fn default() -> Self {
        Self {
            min_replicas: default_min_replicas(),
            max_replicas: default_max_replicas(),
            scale_up_threshold: default_scale_up_threshold(),
            scale_down_threshold: default_scale_down_threshold(),
            cooldown_secs: default_cooldown(),
            service_name: default_service_name(),
        }
    }
}

fn default_min_replicas() -> u32 {
    1
}
fn default_max_replicas() -> u32 {
    10
}
fn default_scale_up_threshold() -> u32 {
    5
}
fn default_scale_down_threshold() -> u32 {
    1
}
fn default_cooldown() -> u64 {
    300
}
fn default_service_name() -> String {
    "workforce-agent-worker".to_string()
}

/// A scaling decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScalingDecision {
    /// Scale up to N replicas.
    ScaleUp(u32),
    /// Scale down to N replicas.
    ScaleDown(u32),
    /// No change needed.
    NoChange,
    /// In cooldown period, skip evaluation.
    Cooldown,
}

/// Metrics snapshot for scaling decisions.
#[derive(Debug, Clone, Default)]
pub struct ScalingMetrics {
    /// Number of tasks waiting in queue.
    pub queue_depth: u32,
    /// Current number of replicas.
    pub current_replicas: u32,
    /// Average task completion time (seconds).
    pub avg_completion_secs: f64,
    /// Seconds since last scaling action.
    pub since_last_scale_secs: u64,
}

/// Evaluates scaling decisions based on metrics.
pub struct ScalingEvaluator {
    config: ScalingConfig,
}

impl ScalingEvaluator {
    pub fn new(config: ScalingConfig) -> Self {
        Self { config }
    }

    /// Evaluate whether to scale up, down, or hold.
    pub fn evaluate(&self, metrics: &ScalingMetrics) -> ScalingDecision {
        // Check cooldown
        if metrics.since_last_scale_secs < self.config.cooldown_secs {
            return ScalingDecision::Cooldown;
        }

        let tasks_per_replica = if metrics.current_replicas == 0 {
            metrics.queue_depth
        } else {
            metrics.queue_depth / metrics.current_replicas
        };

        // Scale up
        if tasks_per_replica >= self.config.scale_up_threshold {
            let desired = self.desired_replicas(metrics);
            if desired > metrics.current_replicas {
                return ScalingDecision::ScaleUp(desired);
            }
        }

        // Scale down
        if tasks_per_replica <= self.config.scale_down_threshold
            && metrics.current_replicas > self.config.min_replicas
        {
            let desired = self.desired_replicas(metrics);
            if desired < metrics.current_replicas {
                return ScalingDecision::ScaleDown(desired);
            }
        }

        ScalingDecision::NoChange
    }

    /// Calculate desired replica count.
    fn desired_replicas(&self, metrics: &ScalingMetrics) -> u32 {
        if metrics.queue_depth == 0 {
            return self.config.min_replicas;
        }

        // Target: each replica handles ~scale_up_threshold/2 tasks
        let target_per_replica = (self.config.scale_up_threshold / 2).max(1);
        let desired = metrics.queue_depth.div_ceil(target_per_replica);

        desired.clamp(self.config.min_replicas, self.config.max_replicas)
    }

    /// Generate a Docker Swarm scale command.
    pub fn scale_command(&self, replicas: u32) -> String {
        format!(
            "docker service scale {}={}",
            self.config.service_name, replicas
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_change_normal_load() {
        let eval = ScalingEvaluator::new(ScalingConfig::default());
        let metrics = ScalingMetrics {
            queue_depth: 3,
            current_replicas: 2,
            since_last_scale_secs: 600,
            ..Default::default()
        };

        assert_eq!(eval.evaluate(&metrics), ScalingDecision::NoChange);
    }

    #[test]
    fn scale_up_high_queue() {
        let eval = ScalingEvaluator::new(ScalingConfig::default());
        let metrics = ScalingMetrics {
            queue_depth: 20,
            current_replicas: 1,
            since_last_scale_secs: 600,
            ..Default::default()
        };

        match eval.evaluate(&metrics) {
            ScalingDecision::ScaleUp(n) => assert!(n > 1),
            other => panic!("Expected ScaleUp, got {:?}", other),
        }
    }

    #[test]
    fn scale_down_idle() {
        let eval = ScalingEvaluator::new(ScalingConfig::default());
        let metrics = ScalingMetrics {
            queue_depth: 0,
            current_replicas: 5,
            since_last_scale_secs: 600,
            ..Default::default()
        };

        match eval.evaluate(&metrics) {
            ScalingDecision::ScaleDown(n) => assert!(n < 5),
            other => panic!("Expected ScaleDown, got {:?}", other),
        }
    }

    #[test]
    fn respects_cooldown() {
        let eval = ScalingEvaluator::new(ScalingConfig::default());
        let metrics = ScalingMetrics {
            queue_depth: 100,
            current_replicas: 1,
            since_last_scale_secs: 10, // Within cooldown
            ..Default::default()
        };

        assert_eq!(eval.evaluate(&metrics), ScalingDecision::Cooldown);
    }

    #[test]
    fn respects_min_replicas() {
        let eval = ScalingEvaluator::new(ScalingConfig {
            min_replicas: 2,
            ..Default::default()
        });

        let replicas = eval.desired_replicas(&ScalingMetrics {
            queue_depth: 0,
            current_replicas: 2,
            ..Default::default()
        });

        assert_eq!(replicas, 2);
    }

    #[test]
    fn respects_max_replicas() {
        let eval = ScalingEvaluator::new(ScalingConfig {
            max_replicas: 5,
            ..Default::default()
        });

        let replicas = eval.desired_replicas(&ScalingMetrics {
            queue_depth: 1000,
            current_replicas: 5,
            ..Default::default()
        });

        assert_eq!(replicas, 5);
    }

    #[test]
    fn scale_command() {
        let eval = ScalingEvaluator::new(ScalingConfig::default());
        let cmd = eval.scale_command(3);
        assert_eq!(cmd, "docker service scale workforce-agent-worker=3");
    }
}
