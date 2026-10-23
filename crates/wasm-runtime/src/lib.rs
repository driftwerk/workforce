//! WASM skill runtime for workforce.
//!
//! This crate provides the infrastructure for loading, caching, and executing
//! WASM-based skills. Skill authors write Rust code using the `workforce-skill-sdk`
//! crate, compile to `wasm32-unknown-unknown`, and distribute via GitHub repos.
//!
//! The runtime exposes host functions (vault, HTTP, logging, config) to WASM
//! guests via the Wasmtime engine, and bridges WASM tool execution into
//! workforce's `Tool` trait so agents can call WASM tools like any built-in tool.

pub mod approval;
pub mod engine;
pub mod host;
pub mod manifest;
pub mod store;
pub mod tool;
pub mod types;

pub use engine::WasmSkillEngine;
pub use manifest::SkillManifest;
pub use store::WasmSkillStore;
pub use tool::WasmSkillTool;
pub use types::{
    Capability, CredentialScope, InstalledWasmSkill, OwnedSkill, SkillOwnership,
    SkillRuntimeBindings, SkillUseGate, SkillUseGateSlot, WasmSkillState, WasmToolDefinition,
};
