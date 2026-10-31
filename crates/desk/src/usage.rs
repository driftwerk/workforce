//! How much of this computer's plans its own Claude Code and Codex report
//! used: the readings the desk passes to the hub. Who each CLI is signed in
//! as is read in [`crate::engine`].

use serde::Serialize;
use serde_json::Value;

/// Which of this computer's CLIs a reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LocalPlan {
    Claude,
    Codex,
}

/// The plan account a CLI on this computer is signed in to: the ids it is
/// known by, raw (the hub keys them), and the plan's name when it says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalAccount {
    pub ids: Vec<String>,
    pub plan: Option<String>,
}

/// Where the desk reports this computer's readings: the hub normalises and
/// keeps them.
pub trait PlanSink: Send + Sync {
    /// `payload` as `plan`'s CLI reported it: Claude Code's `rateLimits`,
    /// Codex's rate-limit snapshot.
    fn local(&self, plan: LocalPlan, payload: Value, account: LocalAccount);
}
