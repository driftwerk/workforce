//! The shapes a playbook shows the hub: what it resolved to and what it did.

use workforce_core::access::Visibility;
use workforce_orchestrator::playbook::PlaybookDefinition;

/// Outcome of resolving a bare playbook id named by an interactive user, who
/// carries no explicit owner (Slack/chat). A member's own playbook shadows a
/// global of the same id; otherwise the single global with that id is used.
pub enum PlaybookResolution {
    Found(PlaybookDefinition),
    NotFound,
    /// More than one *global* playbook shares this bare id — only reachable once
    /// two members publish the same id global; the user must disambiguate.
    Ambiguous,
}

impl PlaybookResolution {
    /// The definition if uniquely resolved, else `None` (for re-fetch sites that
    /// don't need to distinguish not-found from ambiguous).
    pub fn found(self) -> Option<PlaybookDefinition> {
        match self {
            PlaybookResolution::Found(def) => Some(def),
            _ => None,
        }
    }
}

/// A playbook in a listing — the bare id plus its owner (`None` = system) and
/// visibility; the composite key stays an internal addressing detail. Mirrors
/// `WorkflowSummary`.
#[derive(Clone, serde::Serialize)]
pub struct PlaybookSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub step_count: usize,
    pub owner: Option<String>,
    pub visibility: Visibility,
}

/// What the caller (the Slack dispatch loop) should do with a message after the
/// playbook controller has looked at it.
pub enum PlaybookOutcome {
    /// Not a playbook directive — carry on with normal message handling.
    NotHandled,
    /// Directive fully handled; do not run an agent turn.
    Consumed,
    /// Run an agent turn with this injected prompt and friendly display label.
    /// `anchor_ts` is the message id of the `▶ Step N` divider just posted, so
    /// the dispatcher can hang the "working" reactions on it — without this, a
    /// button- or agent-triggered step (which has no user message to react to)
    /// shows the divider then nothing while the agent formulates the step.
    RunTurn {
        message: String,
        display: String,
        anchor_ts: Option<String>,
    },
}

/// A live playbook "control card": the single bot message a step owns while it
/// is active. It hosts the running timer and, once the agent hands back, the
/// Continue/Exit/Finish buttons. Stored on `AgentService::playbook_cards` keyed
/// by `session_key` so the next advance / exit / completion can finalize it
/// (freeze the duration, strip the buttons) — that is what keeps stale buttons
/// from lingering on earlier steps.
#[derive(Debug, Clone)]
pub struct PlaybookCard {
    /// Channel the card was posted to.
    pub channel: String,
    /// Message id of the card — also the anchor for the working reactions.
    pub ts: String,
    /// Thread the card lives in (`None` = posted to the channel root). Kept so the
    /// card can be re-posted into the same thread when the controls follow the
    /// conversation to the foot of the thread on hand-back.
    pub thread_id: Option<String>,
    /// `Step N/M: Title` — the card header. The state emoji (`▶` / `✅`) is added
    /// at render time, not stored here.
    pub header: String,
    /// Wall-clock millis when the step started (mirrors `progress.step_started_at`).
    pub started_at_ms: i64,
    /// Agent-work duration (ms) frozen when the turn handed back; set post-turn.
    /// Kept so the finalized "done in X" matches the awaiting "done in X" — we
    /// freeze on hand-back and never count the user's deliberation time.
    pub work_ms: Option<i64>,
    /// Resolved model the current step runs on (`progress.step_model`), shown on
    /// the card so the user can see when a step crosses a trust boundary (an
    /// OpenRouter route vs. direct Anthropic). `None` when the step has no model.
    pub model: Option<String>,
}
