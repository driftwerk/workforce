//! `settings`: the agent-facing reader of [`super::report`].

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::runner::CURRENT_SESSION_ID;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::session::Session;

use super::{Scope, Viewer};
use crate::authz::IsAdminFn;
use crate::serve::AgentService;

/// The most rows one call returns; a wider ask is cut to it and says so.
const MAX_ROWS: usize = 250;

pub struct SettingsTool {
    service: Arc<AgentService>,
    is_admin: IsAdminFn,
}

impl SettingsTool {
    pub fn new(service: Arc<AgentService>) -> Self {
        Self {
            service,
            is_admin: crate::authz::never_admin(),
        }
    }

    pub fn with_admin_policy(mut self, is_admin: IsAdminFn) -> Self {
        self.is_admin = is_admin;
        self
    }

    /// The session this turn runs in, whose model override and playbook
    /// step the report reads; one that carries neither outside a turn.
    fn session(&self, viewer: &Viewer) -> Session {
        CURRENT_SESSION_ID
            .try_with(Clone::clone)
            .ok()
            .and_then(|id| self.service.sessions.get(&id).ok().flatten())
            .unwrap_or_else(|| {
                Session::new(
                    "settings",
                    viewer.person.as_deref().unwrap_or_default(),
                    self.service.agent_id.as_str(),
                )
            })
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Input {
    Show {
        #[serde(default)]
        section: Option<String>,
    },
    Explain {
        key: String,
    },
}

impl Input {
    fn scope(self) -> (Scope, bool) {
        match self {
            Self::Show { section } => match section.as_deref().map(str::trim) {
                None | Some("") => (Scope::Summary, false),
                Some("all") => (Scope::All, false),
                Some(section) => (Scope::Section(section.to_string()), false),
            },
            Self::Explain { key } => (Scope::Key(key), true),
        }
    }
}

#[async_trait]
impl Tool for SettingsTool {
    fn name(&self) -> &str {
        "settings"
    }

    fn description(&self) -> &str {
        "This agent's effective configuration: each setting's value, where it is set (default, \
         install config, the agent's config.toml, a runtime override, the environment or the \
         vault) and whether it is in effect (active, off, kill_switched, shadow or inert, with \
         the reason). Call it for any question about what is configured (models, vision, \
         transcription, know-how, tools, budgets, keys) instead of reading config files. Secrets \
         are never shown: only whether a key is set and at which level."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["show", "explain"],
                    "description": "show: a section's effective values, or a summary of every \
                                    section. explain: one setting's chain from default to \
                                    runtime, and the winner."
                },
                "section": {
                    "type": "string",
                    "description": "show only: llm, memory, know_how, tools, decision, handoff, \
                                    research, code_context, attachments, budget, credentials, \
                                    slack, …; \"all\" for everything. Omit for the summary, \
                                    which lists the sections and leaves out values at their \
                                    default."
                },
                "key": {
                    "type": "string",
                    "description": "explain only: a setting's config path, e.g. \
                                    llm.vision.model or know_how.min_users, or a path above \
                                    several (llm.vision)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let (scope, chain) = match serde_json::from_value::<Input>(input) {
            Ok(input) => input.scope(),
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("settings takes action \"show\" or \"explain\" (with a key): {e}"),
                    elapsed(),
                ))
            }
        };
        let viewer = Viewer {
            person: crate::user_config::current_identity_id(),
            admin: (self.is_admin)(),
        };
        let session = self.session(&viewer);
        Ok(
            match super::report(&self.service, &viewer, &session, &scope, chain).await {
                Ok(report) => {
                    ToolResult::success(serde_json::to_value(report.capped(MAX_ROWS))?, elapsed())
                }
                Err(refusal) => ToolResult::failure(refusal.to_string(), elapsed()),
            },
        )
    }
}
