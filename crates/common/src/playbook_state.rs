//! Runtime state for an in-flight Playbook.
//!
//! A Playbook is an interactive, human-in-the-loop flow that injects scripted
//! step prompts into a user's normal chat session, gated by Continue/Exit. The
//! progress for an active playbook is hung off `Session::metadata["playbook"]`
//! (see [`crate::session::Session::playbook`]) so it persists with the session
//! and follows thread rollover. The playbook *definitions* live in the
//! `workforce-playbook` crate; only the runtime progress lives here, so `Session` can
//! hold it without depending on `orchestrator`.

use serde::{Deserialize, Serialize};

/// Progress through an active playbook, stored on the owning session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaybookProgress {
    /// The playbook definition id (e.g. `design-impl-review`).
    pub id: String,
    /// Id of the step currently being worked on. Keyed by id (not index) so a
    /// hot-reload or mid-flight edit of the def can't silently re-point a
    /// branching/looping playbook at the wrong step. Resolved against the def
    /// each turn via `workforce_playbook::current_index`.
    #[serde(default)]
    pub step_id: String,
    /// Legacy pre-branching numeric step index. Deserialized from old sessions
    /// (`"step": N`) so they keep their position; it is *preserved* across saves
    /// until the playbook next advances, which resolves it into `step_id` and
    /// clears it. New sessions never carry it, so it is omitted from their JSON.
    #[serde(default, rename = "step", skip_serializing_if = "Option::is_none")]
    pub legacy_step: Option<usize>,
    /// Random token regenerated on every advance. Continue buttons carry the
    /// nonce that was current when they were rendered; a click whose nonce no
    /// longer matches is a stale button from earlier in the thread and is
    /// rejected rather than double-advancing.
    pub nonce: String,
    /// Scratch directory for pinned artifacts, **relative to the agent
    /// workspace** (e.g. `playbooks/<session-id>`). Stored relative so it
    /// survives a redeploy with a different mount, and fixed at start so it
    /// stays stable across session rollover (when `session.id` changes). The
    /// agent writes here via its workspace-sandboxed file tools, and the
    /// controller reads it back by re-joining the live workspace root.
    #[serde(default)]
    pub scratch: String,
    /// Outputs pinned by completed steps, in step order.
    pub artifacts: Vec<ArtifactRef>,
    /// Wall-clock millis when the playbook landed on the current step (set by the
    /// hub on each start/advance). Drives the per-step "time taken" and the
    /// running timer on the step card. Persisted so the duration survives a hub
    /// restart mid-step; omitted from old sessions' JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_started_at: Option<i64>,
    /// Resolved routed model for the current step (e.g. `"claude-opus-4-7"` or
    /// `"openrouter:anthropic/claude-opus-4.7"`), or `None` when the step
    /// declares no model. Re-resolved by the hub on every start/advance and read
    /// by model selection so every turn of the step uses the same model. Scoped
    /// to the playbook: dropped with the progress on exit/completion and carried
    /// over rollover with the rest of the progress, so it never leaks into the
    /// session's own model override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_model: Option<String>,
}

impl PlaybookProgress {
    /// Start a fresh playbook on its first step. `first_step_id` is the id of
    /// `def.steps[0]`; `scratch` is the workspace-relative artifact directory
    /// (see the field docs).
    pub fn new(
        id: impl Into<String>,
        first_step_id: impl Into<String>,
        nonce: impl Into<String>,
        scratch: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            step_id: first_step_id.into(),
            legacy_step: None,
            nonce: nonce.into(),
            scratch: scratch.into(),
            artifacts: Vec::new(),
            step_started_at: None,
            step_model: None,
        }
    }
}

/// A pointer to a step's pinned output on disk, plus a cheap synopsis used to
/// keep the artifact present in the system prompt across compaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactRef {
    /// The step that produced this artifact.
    pub step_id: String,
    /// Path to the pinned file, relative to the workspace root.
    pub path: String,
    /// First heading / leading excerpt of the file (no LLM call), shown in the
    /// "Active Playbook" prompt block so the agent never forgets it exists.
    pub synopsis: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh session serializes `step_id` and omits the legacy `step` key.
    #[test]
    fn new_progress_omits_legacy_step() {
        let p = PlaybookProgress::new("p", "design", "n0", "playbooks/s");
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["step_id"], "design");
        assert!(json.get("step").is_none());
    }

    /// `step_model` defaults to None, is omitted from JSON when unset, and
    /// survives a round-trip when set — so old sessions deserialize unchanged.
    #[test]
    fn step_model_defaults_none_and_round_trips() {
        let mut p = PlaybookProgress::new("p", "design", "n0", "playbooks/s");
        assert_eq!(p.step_model, None);
        assert!(serde_json::to_value(&p)
            .unwrap()
            .get("step_model")
            .is_none());

        p.step_model = Some("claude-opus-4-7".to_string());
        let round: PlaybookProgress =
            serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(round.step_model.as_deref(), Some("claude-opus-4-7"));
    }

    /// A pre-branching session (numeric `step`, no `step_id`) keeps its position
    /// across a save that doesn't advance the playbook — `legacy_step` must
    /// survive the round-trip rather than silently resetting to step 0.
    #[test]
    fn legacy_step_survives_resave_until_migrated() {
        let old = serde_json::json!({
            "id": "p",
            "step": 2,
            "nonce": "n0",
            "scratch": "playbooks/s",
            "artifacts": [],
        });
        let p: PlaybookProgress = serde_json::from_value(old).unwrap();
        assert_eq!(p.step_id, "");
        assert_eq!(p.legacy_step, Some(2));

        let reserialized = serde_json::to_value(&p).unwrap();
        assert_eq!(reserialized["step"], 2);
        let p2: PlaybookProgress = serde_json::from_value(reserialized).unwrap();
        assert_eq!(p2.legacy_step, Some(2));
    }
}
