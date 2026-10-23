//! What a decision is for, and how long it may take. Every decision sits on
//! a turn's or a step's critical path, so each call carries a time budget
//! that covers waiting for a free slot on the backend and the request
//! itself, and is counted under the use it serves.

use crate::client::DecideError;
use std::time::Duration;

/// The uses a decision is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Use {
    /// A branching workflow step that ended with no route of its own.
    BranchFallback,
    /// A workflow step that is nothing but a question.
    DecisionStep,
    /// An agent step's outcome, asked before its turn runs.
    DecideFirst,
    /// The `decide.ask` agent tool.
    Tool,
    /// The pre-agent on-topic gate.
    ScopeGate,
    /// Classifying a request's use case for a router.
    Routing,
}

impl Use {
    pub fn as_str(self) -> &'static str {
        match self {
            Use::BranchFallback => "branch_fallback",
            Use::DecisionStep => "decision_step",
            Use::DecideFirst => "decide_first",
            Use::Tool => "tool",
            Use::ScopeGate => "scope_gate",
            Use::Routing => "routing",
        }
    }

    /// The time budget when config sets none. Measured on one L4 GPU
    /// serving Kev-4B: one-line states answer at ~33/s, ~3,500-character
    /// ones at 5–6/s with p50 1.4 s at 8 in flight and 3–4 s at 16. Routing
    /// and the scope gate run before every turn, so they give up early and
    /// let the turn proceed, as does a step asked before its own turn; a
    /// step can afford to wait.
    pub fn default_budget(self) -> Duration {
        match self {
            Use::Routing => Duration::from_millis(300),
            Use::ScopeGate | Use::DecideFirst => Duration::from_millis(1_500),
            Use::BranchFallback => Duration::from_secs(5),
            Use::DecisionStep | Use::Tool => Duration::from_secs(10),
        }
    }
}

/// One call: its use and the time it has, queueing included, on each
/// backend it is put to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Call {
    pub usage: Use,
    pub budget: Duration,
}

impl Call {
    pub fn new(usage: Use, budget: Duration) -> Self {
        Self { usage, budget }
    }

    /// `usage` with its default budget.
    pub fn of(usage: Use) -> Self {
        Self::new(usage, usage.default_budget())
    }

    /// Count one attempt on `backend`: how it ended, how long it queued for
    /// a slot and, when a request was sent, how long that took.
    pub(crate) fn observe<T>(
        &self,
        backend: &str,
        queued: Duration,
        request: Option<Duration>,
        result: &Result<T, DecideError>,
    ) {
        let usage = self.usage.as_str();
        let outcome = match result {
            Ok(_) => "ok",
            Err(DecideError::Busy { .. }) => "busy",
            Err(DecideError::Timeout { .. }) => "timeout",
            Err(_) => "error",
        };
        let queue_ms = queued.as_secs_f64() * 1_000.0;
        let latency_ms = request.map(|r| r.as_secs_f64() * 1_000.0);
        metrics::counter!(
            "workforce.decision.calls",
            "use" => usage,
            "backend" => backend.to_string(),
            "result" => outcome,
        )
        .increment(1);
        metrics::histogram!(
            "workforce.decision.queue_ms",
            "use" => usage,
            "backend" => backend.to_string(),
        )
        .record(queue_ms);
        if let Some(latency_ms) = latency_ms {
            metrics::histogram!(
                "workforce.decision.latency_ms",
                "use" => usage,
                "backend" => backend.to_string(),
            )
            .record(latency_ms);
        }
        tracing::debug!(
            target: "decision",
            r#use = usage,
            backend,
            result = outcome,
            queue_ms,
            latency_ms,
            budget_ms = self.budget.as_millis() as u64,
            "decision call"
        );
    }

    /// Count the fallback stepping in for a primary that could not answer.
    pub(crate) fn fell_back(&self, fallback: &str, reason: &'static str) {
        metrics::counter!(
            "workforce.decision.fallbacks",
            "use" => self.usage.as_str(),
            "backend" => fallback.to_string(),
            "reason" => reason,
        )
        .increment(1);
    }
}
