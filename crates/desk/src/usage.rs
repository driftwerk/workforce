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

    /// The windows `payload` says: how full each is and when it resets.
    /// Desk reads them to know when an account has hit its limit.
    fn windows(&self, _plan: LocalPlan, _payload: &Value) -> Vec<LimitWindow> {
        Vec::new()
    }

    /// The key readings of the account known by `ids` are kept under, as
    /// the Floor's plan cards group them.
    fn account_key(&self, _plan: LocalPlan, _ids: &[String]) -> Option<String> {
        None
    }
}

/// One of a plan's windows as a CLI reported it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimitWindow {
    pub used_percent: f64,
    pub resets_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Its length: 300 for the 5-hour window, 10 080 for the weekly one.
    pub minutes: u32,
}
