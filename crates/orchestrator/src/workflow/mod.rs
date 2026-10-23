//! TOML-defined workflow engine — step-by-step task execution with branching,
//! pause/resume, and handoff integration.
//!
//! Complements SKILL.md (skills = how, workflows = what order) for complex
//! multi-session tasks: investigations, incident response, multi-agent handoffs.

pub mod decision;
pub mod definition;
pub mod engine;
pub mod instance;
pub mod package;
pub mod report;
pub mod template;
