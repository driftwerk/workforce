//! Playbooks — interactive, human-in-the-loop flows.
//!
//! Sibling to the orchestrator's workflows: where a workflow runs a fresh agent
//! per step and hands context forward as variables, a playbook injects scripted
//! step prompts into one persistent chat session, gated by the user
//! (Continue/Exit). Definitions and the pure stepping engine live here; the
//! runtime progress lives on the session
//! (`workforce_common::playbook_state::PlaybookProgress`) and the dispatch
//! wiring lives in each host (hub channels, edge widget).
//!
//! This crate is deliberately IO-free (no DB, no network) so hardened,
//! internet-facing binaries like the edge can link it without dragging in the
//! orchestrator's scheduler or workflow engine.

pub mod definition;
pub mod engine;

pub use definition::{
    ButtonEffect, ButtonStyle, PlaybookButton, PlaybookDefinition, PlaybookMeta, PlaybookStep,
};
pub use engine::{
    active_playbook_block, advance, advance_to, current_index, finish, render_step_prompt,
    synopsis_of, StepOutcome,
};
