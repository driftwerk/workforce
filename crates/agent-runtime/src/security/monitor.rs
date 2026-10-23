//! Post-execution behavioural anomaly monitor.
//!
//! Tracks tool usage patterns after untrusted content is processed and flags
//! suspicious sequences: data exfiltration, identity tampering, privilege
//! escalation, and credential access.

use std::collections::VecDeque;
use std::time::Instant;

use tracing::warn;

// ─── Anomaly Signals ─────────────────────────────────────────────────────────

/// A detected anomalous behaviour.
#[derive(Debug, Clone)]
pub enum AnomalySignal {
    /// File read followed by outbound message — potential data leak.
    DataExfiltration {
        read_tool: String,
        send_tool: String,
    },
    /// Write or modification of identity files.
    IdentityTampering { file: String },
    /// Access to system or policy administration tools.
    PrivilegeEscalation { tool: String },
    /// Access to vault or credential storage.
    CredentialAccess { tool: String },
    /// Unusual tool sequence not seen in normal operation.
    UnusualToolSequence { tools: Vec<String> },
}

impl AnomalySignal {
    /// Risk weight for this signal type.
    pub fn weight(&self) -> f64 {
        match self {
            AnomalySignal::DataExfiltration { .. } => 0.5,
            AnomalySignal::IdentityTampering { .. } => 0.8,
            AnomalySignal::PrivilegeEscalation { .. } => 0.6,
            AnomalySignal::CredentialAccess { .. } => 0.7,
            AnomalySignal::UnusualToolSequence { .. } => 0.3,
        }
    }

    /// Human-readable description.
    pub fn description(&self) -> String {
        match self {
            AnomalySignal::DataExfiltration {
                read_tool,
                send_tool,
            } => {
                format!("Data exfiltration pattern: {} → {}", read_tool, send_tool)
            }
            AnomalySignal::IdentityTampering { file } => {
                format!("Identity file modification: {}", file)
            }
            AnomalySignal::PrivilegeEscalation { tool } => {
                format!("Privilege escalation via: {}", tool)
            }
            AnomalySignal::CredentialAccess { tool } => {
                format!("Credential access via: {}", tool)
            }
            AnomalySignal::UnusualToolSequence { tools } => {
                format!("Unusual tool sequence: {}", tools.join(" → "))
            }
        }
    }
}

// ─── Tool Record ─────────────────────────────────────────────────────────────

/// Record of a single tool invocation for pattern tracking.
#[derive(Debug, Clone)]
struct ToolRecord {
    tool_name: String,
    #[allow(dead_code)]
    timestamp: Instant,
}

// ─── Tool Classification ─────────────────────────────────────────────────────

/// Tools that read data (potential exfiltration sources).
const READ_TOOLS: &[&str] = &[
    "code.read",
    "code.search",
    "code.tree",
    "code.symbols",
    "memory.search",
    "memory.recall",
    "vault.get",
    "vault.list",
];

/// Tools that send data externally (potential exfiltration sinks).
const SEND_TOOLS: &[&str] = &[
    "messaging.send",
    "messaging.edit",
    "messaging.upload_file",
    "reaction.add",
];

/// Identity files that must not be tampered with.
const IDENTITY_FILES: &[&str] = &[
    "SOUL.md",
    "AGENTS.md",
    "IDENTITY.md",
    "PERSONALITY.md",
    "GOALS.md",
];

/// Tools that indicate privilege escalation attempts.
const ADMIN_TOOLS: &[&str] = &[
    "system.enable_skill",
    "system.disable_skill",
    "system.save_config",
    "policy.propose",
];

/// Tools that access credentials.
const CREDENTIAL_TOOLS: &[&str] = &["vault.get", "vault.list", "vault.set", "vault.delete"];

// ─── Session Risk Tracker ────────────────────────────────────────────────────

/// Tracks tool usage and detects anomalous patterns after untrusted content.
#[derive(Debug)]
pub struct SessionRiskTracker {
    /// When untrusted content was last processed.
    untrusted_content_at: Option<Instant>,
    /// Recent tool invocations (ring buffer).
    tool_window: VecDeque<ToolRecord>,
    /// Cumulative risk score for the monitoring window.
    risk_score: f64,
    /// All signals detected in the current monitoring window.
    signals: Vec<AnomalySignal>,
    /// Number of tool calls to monitor after untrusted content (default 10).
    cooldown_calls: u32,
    /// Counter of tool calls since untrusted content.
    calls_since_untrusted: u32,
    /// Maximum time window for monitoring (5 minutes).
    max_window_secs: u64,
}

/// Risk threshold for flagging a session.
const RISK_THRESHOLD: f64 = 0.7;

impl Default for SessionRiskTracker {
    fn default() -> Self {
        Self::new(10)
    }
}

impl SessionRiskTracker {
    /// Create a new tracker with a given cooldown window size.
    pub fn new(cooldown_calls: u32) -> Self {
        Self {
            untrusted_content_at: None,
            tool_window: VecDeque::with_capacity(20),
            risk_score: 0.0,
            signals: Vec::new(),
            cooldown_calls,
            calls_since_untrusted: 0,
            max_window_secs: 300, // 5 minutes
        }
    }

    /// Mark that untrusted content was just processed — start monitoring window.
    pub fn on_untrusted_content(&mut self) {
        self.untrusted_content_at = Some(Instant::now());
        self.calls_since_untrusted = 0;
        self.risk_score = 0.0;
        self.signals.clear();
    }

    /// Whether the tracker is currently in a monitoring window.
    pub fn is_monitoring(&self) -> bool {
        if let Some(at) = self.untrusted_content_at {
            self.calls_since_untrusted < self.cooldown_calls
                && at.elapsed().as_secs() < self.max_window_secs
        } else {
            false
        }
    }

    /// Current cumulative risk score.
    pub fn risk_score(&self) -> f64 {
        self.risk_score
    }

    /// Whether the risk score exceeds the threshold.
    pub fn is_high_risk(&self) -> bool {
        self.risk_score >= RISK_THRESHOLD
    }

    /// All signals detected so far.
    pub fn signals(&self) -> &[AnomalySignal] {
        &self.signals
    }

    /// Called after each tool execution. Returns new signals detected, if any.
    ///
    /// The `input` parameter is the tool's JSON input — used for detecting
    /// identity file paths in write operations.
    pub fn on_tool_executed(
        &mut self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Option<Vec<AnomalySignal>> {
        // Record the tool call
        self.tool_window.push_back(ToolRecord {
            tool_name: tool_name.to_string(),
            timestamp: Instant::now(),
        });
        if self.tool_window.len() > 20 {
            self.tool_window.pop_front();
        }

        // Not in monitoring window → skip
        if !self.is_monitoring() {
            return None;
        }

        self.calls_since_untrusted += 1;
        let mut new_signals = Vec::new();

        // ── Check: data exfiltration ─────────────────────────────────────
        if SEND_TOOLS.contains(&tool_name) {
            // Look back for recent read tools
            for record in self.tool_window.iter().rev().skip(1) {
                if READ_TOOLS.contains(&record.tool_name.as_str()) {
                    new_signals.push(AnomalySignal::DataExfiltration {
                        read_tool: record.tool_name.clone(),
                        send_tool: tool_name.to_string(),
                    });
                    break;
                }
            }
        }

        // ── Check: identity tampering ────────────────────────────────────
        if tool_name == "code.edit" || tool_name == "code.write" {
            if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
                for identity_file in IDENTITY_FILES {
                    if path.ends_with(identity_file) {
                        new_signals.push(AnomalySignal::IdentityTampering {
                            file: identity_file.to_string(),
                        });
                    }
                }
            }
        }

        // ── Check: privilege escalation ──────────────────────────────────
        if ADMIN_TOOLS.contains(&tool_name) {
            new_signals.push(AnomalySignal::PrivilegeEscalation {
                tool: tool_name.to_string(),
            });
        }

        // ── Check: credential access ─────────────────────────────────────
        if CREDENTIAL_TOOLS.contains(&tool_name) {
            new_signals.push(AnomalySignal::CredentialAccess {
                tool: tool_name.to_string(),
            });
        }

        // Update risk score
        for signal in &new_signals {
            self.risk_score += signal.weight();
            self.signals.push(signal.clone());
        }

        // Log anomaly signals
        for signal in &new_signals {
            warn!(
                target: "sentinel.monitor",
                tool = %tool_name,
                signal = %signal.description(),
                risk_score = format!("{:.2}", self.risk_score),
                "Anomaly signal detected"
            );
        }
        if self.is_high_risk() {
            warn!(
                target: "sentinel.monitor",
                risk_score = format!("{:.2}", self.risk_score),
                signal_count = self.signals.len(),
                "Session risk threshold exceeded"
            );
        }

        if new_signals.is_empty() {
            None
        } else {
            Some(new_signals)
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_monitoring_without_untrusted_content() {
        let mut tracker = SessionRiskTracker::default();
        let result = tracker.on_tool_executed("code.read", &serde_json::json!({"path": "main.rs"}));
        assert!(result.is_none());
        assert!(!tracker.is_monitoring());
    }

    #[test]
    fn monitoring_starts_after_untrusted_content() {
        let mut tracker = SessionRiskTracker::default();
        tracker.on_untrusted_content();
        assert!(tracker.is_monitoring());
    }

    #[test]
    fn exfiltration_pattern_detected() {
        let mut tracker = SessionRiskTracker::default();
        tracker.on_untrusted_content();

        // Read a file
        tracker.on_tool_executed("code.read", &serde_json::json!({"path": "secrets.txt"}));

        // Then send a message
        let signals = tracker
            .on_tool_executed(
                "messaging.send",
                &serde_json::json!({"channel": "C123", "text": "data"}),
            )
            .unwrap();

        assert!(signals
            .iter()
            .any(|s| matches!(s, AnomalySignal::DataExfiltration { .. })));
    }

    #[test]
    fn identity_tampering_detected() {
        let mut tracker = SessionRiskTracker::default();
        tracker.on_untrusted_content();

        let signals = tracker
            .on_tool_executed(
                "code.edit",
                &serde_json::json!({"path": "/workspace/SOUL.md", "content": "hacked"}),
            )
            .unwrap();

        assert!(signals
            .iter()
            .any(|s| matches!(s, AnomalySignal::IdentityTampering { .. })));
    }

    #[test]
    fn privilege_escalation_detected() {
        let mut tracker = SessionRiskTracker::default();
        tracker.on_untrusted_content();

        let signals = tracker
            .on_tool_executed(
                "system.enable_skill",
                &serde_json::json!({"skill": "github"}),
            )
            .unwrap();

        assert!(signals
            .iter()
            .any(|s| matches!(s, AnomalySignal::PrivilegeEscalation { .. })));
    }

    #[test]
    fn credential_access_detected() {
        let mut tracker = SessionRiskTracker::default();
        tracker.on_untrusted_content();

        let signals = tracker
            .on_tool_executed("vault.get", &serde_json::json!({"key": "api_key"}))
            .unwrap();

        assert!(signals
            .iter()
            .any(|s| matches!(s, AnomalySignal::CredentialAccess { .. })));
    }

    #[test]
    fn cooldown_expires() {
        let mut tracker = SessionRiskTracker::new(2);
        tracker.on_untrusted_content();

        // Two calls exhaust the cooldown
        tracker.on_tool_executed("code.read", &serde_json::json!({}));
        tracker.on_tool_executed("code.read", &serde_json::json!({}));

        assert!(!tracker.is_monitoring());

        // After cooldown, no detection
        let result = tracker.on_tool_executed("vault.get", &serde_json::json!({"key": "secret"}));
        assert!(result.is_none());
    }

    #[test]
    fn risk_score_accumulates() {
        let mut tracker = SessionRiskTracker::default();
        tracker.on_untrusted_content();

        // Credential access (0.7)
        tracker.on_tool_executed("vault.get", &serde_json::json!({}));
        assert!((tracker.risk_score() - 0.7).abs() < 0.01);
        assert!(tracker.is_high_risk());
    }

    #[test]
    fn safe_tools_produce_no_signals() {
        let mut tracker = SessionRiskTracker::default();
        tracker.on_untrusted_content();

        let result =
            tracker.on_tool_executed("code.read", &serde_json::json!({"path": "readme.md"}));
        // code.read alone is not suspicious
        assert!(result.is_none());
    }

    #[test]
    fn normal_edit_not_flagged() {
        let mut tracker = SessionRiskTracker::default();
        tracker.on_untrusted_content();

        // Edit a normal file — not identity
        let result = tracker.on_tool_executed(
            "code.edit",
            &serde_json::json!({"path": "/workspace/src/main.rs"}),
        );
        assert!(result.is_none());
    }

    #[test]
    fn signal_descriptions() {
        let s = AnomalySignal::DataExfiltration {
            read_tool: "code.read".into(),
            send_tool: "messaging.send".into(),
        };
        assert!(s.description().contains("code.read"));
        assert!(s.description().contains("messaging.send"));
    }
}
