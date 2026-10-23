//! Pure playbook state-machine helpers.
//!
//! All IO (reading artifact files, saving the session, posting to Slack) lives
//! in the hub; this module only computes — so it stays trivially testable, the
//! same split the workflow engine uses.

use workforce_common::playbook_state::{ArtifactRef, PlaybookProgress};

use super::definition::PlaybookDefinition;

/// What to do after a Continue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// Inject the prompt for this step index.
    Run(usize),
    /// No steps remain — the playbook is complete.
    Complete,
}

/// Resolve the index of the step the playbook is currently on, against `def`.
///
/// Read-only: it does not mutate `progress`. A pre-branching session carries a
/// legacy numeric `step` instead of a `step_id`; that falls back to the index
/// directly here, and is folded into `step_id` the next time the playbook
/// advances (which writes the new step's id). Returns `None` if the id is
/// unknown — e.g. the step was deleted from the def mid-flight.
pub fn current_index(progress: &PlaybookProgress, def: &PlaybookDefinition) -> Option<usize> {
    if !progress.step_id.is_empty() {
        return def.steps.iter().position(|s| s.id == progress.step_id);
    }
    progress.legacy_step.filter(|i| *i < def.steps.len())
}

/// Pin the just-completed step's artifact (deduped by step id, keeping the
/// latest so a loop like review→impl→review can't bloat the list) and reset the
/// nonce. Shared prologue for every advance path.
fn complete_step(
    progress: &mut PlaybookProgress,
    completed_artifact: Option<ArtifactRef>,
    new_nonce: String,
) {
    if let Some(artifact) = completed_artifact {
        progress.artifacts.retain(|a| a.step_id != artifact.step_id);
        progress.artifacts.push(artifact);
    }
    progress.nonce = new_nonce;
}

/// Land on `idx`, recording its id as the new current step. An out-of-range
/// index means there is nothing left to run → `Complete`.
fn land_on(progress: &mut PlaybookProgress, def: &PlaybookDefinition, idx: usize) -> StepOutcome {
    // Any landing writes a concrete `step_id`, completing a legacy migration.
    progress.legacy_step = None;
    match def.steps.get(idx) {
        Some(step) => {
            progress.step_id = step.id.clone();
            StepOutcome::Run(idx)
        }
        None => StepOutcome::Complete,
    }
}

/// Advance to the next *sequential* step — the default linear `Continue` edge.
pub fn advance(
    progress: &mut PlaybookProgress,
    def: &PlaybookDefinition,
    completed_artifact: Option<ArtifactRef>,
    new_nonce: String,
) -> StepOutcome {
    let next = current_index(progress, def).map_or(def.steps.len(), |i| i + 1);
    complete_step(progress, completed_artifact, new_nonce);
    land_on(progress, def, next)
}

/// Jump to a specific step id — a custom button's labeled edge (branch / goto /
/// skip / loop-back). If the target no longer exists (shouldn't happen — save
/// validates targets and a broken edit drops the whole def), stay on the
/// current step rather than silently ending the playbook.
pub fn advance_to(
    progress: &mut PlaybookProgress,
    def: &PlaybookDefinition,
    target_id: &str,
    completed_artifact: Option<ArtifactRef>,
    new_nonce: String,
) -> StepOutcome {
    let target = def
        .steps
        .iter()
        .position(|s| s.id == target_id)
        .or_else(|| current_index(progress, def));
    complete_step(progress, completed_artifact, new_nonce);
    match target {
        Some(idx) => land_on(progress, def, idx),
        None => StepOutcome::Complete,
    }
}

/// Terminate the playbook as *complete* — a custom `finish` button. Pins the
/// final artifact and bumps the nonce, then reports `Complete`.
pub fn finish(
    progress: &mut PlaybookProgress,
    completed_artifact: Option<ArtifactRef>,
    new_nonce: String,
) -> StepOutcome {
    complete_step(progress, completed_artifact, new_nonce);
    StepOutcome::Complete
}

/// Render the prompt to inject for `step_idx`: the step's own prompt, plus an
/// auto-appended instruction to write its artifact (when declared) and a list
/// of prior artifacts to read for detail. The artifact path is the
/// workspace-relative `progress.scratch`, which the agent's file tools resolve
/// under the workspace root.
pub fn render_step_prompt(
    def: &PlaybookDefinition,
    step_idx: usize,
    progress: &PlaybookProgress,
) -> Option<String> {
    let step = def.steps.get(step_idx)?;
    let mut out = step.prompt.clone();
    if let Some(artifact) = &step.artifact {
        out.push_str(&format!(
            "\n\n---\nWhen this step is complete, write your output to `{}/{artifact}` \
             so later steps in this playbook can build on it.",
            progress.scratch
        ));
    }
    if !progress.artifacts.is_empty() {
        out.push_str("\n\nPrior step outputs (read these for detail):");
        for a in &progress.artifacts {
            out.push_str(&format!("\n- {}: `{}`", a.step_id, a.path));
        }
    }
    // Scope boundary — the last thing the agent reads, so it sticks. Playbook
    // steps are gated on the user, but nothing stops the agent free-running into
    // later steps (creating the ticket during design, coding during the ticket
    // step) unless we say so on every step. Naming the *next* step makes the
    // boundary concrete.
    out.push_str("\n\n---\n");
    match def.steps.get(step_idx + 1) {
        Some(next) => out.push_str(&format!(
            "Stay within THIS step only. Do not start, prepare, or get a head start on \"{}\" \
             or any later step. When this step's work is done, stop, briefly summarise what you \
             did, and wait for the user to press Continue — they decide when the playbook advances.",
            next.title
        )),
        None => out.push_str(
            "This is the final step. When its work is done, stop, briefly summarise what you did, \
             and wait for the user to press Finish — take no further action on your own.",
        ),
    }
    Some(out)
}

/// The overview posted when a playbook starts: the full list of steps (so the
/// user sees up front how many there are and what they cover) before step 1
/// runs. Pure formatting; the hub posts it to the thread.
pub fn render_overview(def: &PlaybookDefinition) -> String {
    let total = def.steps.len();
    let plural = if total == 1 { "" } else { "s" };
    let mut out = format!(
        "▶ *Starting playbook: {}* — {total} step{plural}\n",
        def.playbook.name
    );
    for (i, step) in def.steps.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, step.title));
    }
    if let Some(first) = def.steps.first() {
        out.push_str(&format!("\n_Diving into step 1: {}…_", first.title));
    }
    out
}

/// The "Active Playbook" block injected into the system prompt every turn, so
/// the agent never loses awareness of the playbook or its pinned artifacts even
/// after the raw discussion is compacted away.
pub fn active_playbook_block(def: &PlaybookDefinition, progress: &PlaybookProgress) -> String {
    let total = def.steps.len();
    let idx = current_index(progress, def).unwrap_or(0);
    let current = idx + 1;
    let title = def.steps.get(idx).map(|s| s.title.as_str()).unwrap_or("");
    let mut out = format!(
        "\n\n## Active Playbook: {} — Step {current}/{total}: {title}\n\
         You are running an interactive playbook. Prior step outputs are pinned on disk; \
         read them when you need the detail:\n",
        def.playbook.name
    );
    if progress.artifacts.is_empty() {
        out.push_str("- (none pinned yet)\n");
    } else {
        for a in &progress.artifacts {
            out.push_str(&format!("- {}: `{}` — {}\n", a.step_id, a.path, a.synopsis));
        }
    }
    out.push_str(
        "Work strictly within the current step. Do NOT perform, prepare, or get a head start on \
         anything that belongs to a later step — for example, don't create the ticket while you're \
         still in the design step, and don't start editing code during the ticket step. A step is \
         finished only when the work it describes is done; at that point stop, briefly summarise, \
         and hand back to the user. The user drives the playbook with the Continue/Exit/Finish \
         buttons, so let them click. You may advance or exit it yourself with the `playbook` tool \
         (action `continue` / `exit`) only when the user explicitly tells you to — say so in one \
         line when you do.\n",
    );
    out
}

/// Cheap, LLM-free synopsis of a pinned artifact: the first markdown heading,
/// else the leading words. Used to keep the artifact present in the prompt.
pub fn synopsis_of(content: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let heading = rest.trim_start_matches('#').trim();
            if !heading.is_empty() {
                return truncate(heading, 200);
            }
        }
    }
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&collapsed, 200)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def() -> PlaybookDefinition {
        PlaybookDefinition::from_toml(
            r#"
[playbook]
id = "p"
name = "P"
[[step]]
id = "design"
title = "Design"
artifact = "design.md"
prompt = "Design it."
[[step]]
id = "implement"
title = "Implement"
prompt = "Build it."
"#,
        )
        .unwrap()
    }

    #[test]
    fn advance_runs_next_then_completes() {
        let d = def();
        let mut p = PlaybookProgress::new("p", "design", "n0", "playbooks/sess");
        let art = ArtifactRef {
            step_id: "design".into(),
            path: "scratch/design.md".into(),
            synopsis: "The design".into(),
        };
        let out = advance(&mut p, &d, Some(art), "n1".into());
        assert_eq!(out, StepOutcome::Run(1));
        assert_eq!(p.step_id, "implement");
        assert_eq!(p.nonce, "n1");
        assert_eq!(p.artifacts.len(), 1);

        let out = advance(&mut p, &d, None, "n2".into());
        assert_eq!(out, StepOutcome::Complete);
    }

    #[test]
    fn advance_to_jumps_and_loops() {
        let d = def();
        let mut p = PlaybookProgress::new("p", "implement", "n0", "playbooks/sess");
        // Loop back to an earlier step.
        let out = advance_to(&mut p, &d, "design", None, "n1".into());
        assert_eq!(out, StepOutcome::Run(0));
        assert_eq!(p.step_id, "design");
    }

    #[test]
    fn advance_to_dedupes_artifacts_by_step() {
        let d = def();
        let mut p = PlaybookProgress::new("p", "design", "n0", "playbooks/sess");
        let art = || ArtifactRef {
            step_id: "design".into(),
            path: "playbooks/sess/design.md".into(),
            synopsis: "v1".into(),
        };
        advance(&mut p, &d, Some(art()), "n1".into());
        // Loop back and re-pin the same step's artifact — must not duplicate.
        advance_to(&mut p, &d, "design", None, "n2".into());
        advance(&mut p, &d, Some(art()), "n3".into());
        assert_eq!(
            p.artifacts.iter().filter(|a| a.step_id == "design").count(),
            1
        );
    }

    #[test]
    fn legacy_numeric_step_resolves() {
        let d = def();
        // Simulate a pre-branching session: no step_id, legacy index 1.
        let mut p = PlaybookProgress::new("p", "design", "n0", "playbooks/sess");
        p.step_id = String::new();
        p.legacy_step = Some(1);
        assert_eq!(current_index(&p, &d), Some(1));
        // Advancing folds the legacy index into a real step_id.
        let out = advance(&mut p, &d, None, "n1".into());
        assert_eq!(out, StepOutcome::Complete);
    }

    #[test]
    fn step_prompt_appends_artifact_instruction_and_priors() {
        let d = def();
        let mut p = PlaybookProgress::new("p", "design", "n0", "playbooks/sess");
        // Step 0 declares an artifact → write instruction appended.
        let s0 = render_step_prompt(&d, 0, &p).unwrap();
        assert!(s0.contains("Design it."));
        assert!(s0.contains("playbooks/sess/design.md"));
        assert!(!s0.contains("Prior step outputs"));

        // After pinning step 0, step 1 lists it as a prior.
        p.artifacts.push(ArtifactRef {
            step_id: "design".into(),
            path: "playbooks/sess/design.md".into(),
            synopsis: "x".into(),
        });
        p.step_id = "implement".into();
        let s1 = render_step_prompt(&d, 1, &p).unwrap();
        assert!(s1.contains("Prior step outputs"));
        assert!(s1.contains("design.md"));
    }

    #[test]
    fn step_prompt_appends_scope_guardrail_naming_next_step() {
        let d = def();
        let p = PlaybookProgress::new("p", "design", "n0", "playbooks/sess");
        // A non-final step names the next step and tells the agent to stop.
        let s0 = render_step_prompt(&d, 0, &p).unwrap();
        assert!(s0.contains("Stay within THIS step only"));
        assert!(s0.contains("Implement")); // next step's title
        assert!(s0.contains("wait for the user to press Continue"));
        // The final step tells the agent to wait for Finish, no next step named.
        let s1 = render_step_prompt(&d, 1, &p).unwrap();
        assert!(s1.contains("final step"));
        assert!(s1.contains("press Finish"));
    }

    #[test]
    fn active_block_carries_scope_discipline() {
        let d = def();
        let p = PlaybookProgress::new("p", "design", "n0", "playbooks/sess");
        let block = active_playbook_block(&d, &p);
        assert!(block.contains("Work strictly within the current step"));
        assert!(block.contains("don't create the ticket"));
    }

    #[test]
    fn overview_lists_every_step_and_dives_into_first() {
        let d = def();
        let o = render_overview(&d);
        assert!(o.contains("Starting playbook: P"));
        assert!(o.contains("2 steps"));
        assert!(o.contains("1. Design"));
        assert!(o.contains("2. Implement"));
        assert!(o.contains("Diving into step 1: Design"));
    }

    #[test]
    fn active_block_reports_position_and_artifacts() {
        let d = def();
        let p = PlaybookProgress::new("p", "design", "n0", "playbooks/sess");
        let block = active_playbook_block(&d, &p);
        assert!(block.contains("Step 1/2: Design"));
        assert!(block.contains("none pinned yet"));
    }

    #[test]
    fn synopsis_prefers_heading() {
        assert_eq!(synopsis_of("# Title\nbody"), "Title");
        assert_eq!(synopsis_of("## My Plan\nmore"), "My Plan");
        assert_eq!(synopsis_of("just some words here"), "just some words here");
    }

    #[test]
    fn synopsis_truncates() {
        let long = "word ".repeat(100);
        let s = synopsis_of(&long);
        assert!(s.chars().count() <= 200);
        assert!(s.ends_with('…'));
    }
}
