//! The agent service as the hub composes it: core's service, and the
//! composition that builds and wires it.

pub use workforce_hub_core::serve::*;

pub mod background;
mod compose;
#[cfg(test)]
mod composed_tests;
#[cfg(test)]
pub mod testing;
#[cfg(test)]
mod tool_tests;
pub use crate::workflow_runtime::shared_workflow_store;
pub use compose::Compose;
pub use compose::{
    artifact_index_pace, artifact_search_wait, create_trigger_router, create_webhook_router,
    load_policy, open_artifact_store, reindex_artifacts, shared_artifact_store,
    sweep_identity_revisions,
};
pub use compose::{
    artifact_mirror_in_step, artifact_mirror_state, artifact_shadow, mirror_index_pace,
    open_artifact_mirror,
};
pub use compose::{
    register_advisor_tool, register_chatgpt_login_tool, register_claude_code_setup_token_tool,
    register_claude_code_tool, register_codex_setup_auth_tool, register_coding_session_tool,
    wire_agent_jobs,
};
