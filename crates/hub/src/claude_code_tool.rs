//! Ad-hoc Claude Code tool — `coding.claude`.
//!
//! Allows any agent to delegate a complex coding task to Claude Code CLI.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use workforce_agent::tools::exec::validate_workdir;
use workforce_agent::tools::{Tool, ToolResult};

use workforce_common::subscription::{SubscriptionKind, SubscriptionRoute, SCHEDULER};

use crate::agent_turn::connect::no_login_notice;
use crate::claude_code::{self, ClaudeCodeConfig};
use crate::coding_engine::{labelled_model, RoundContext};
use crate::serve::AgentService;

const DEFAULT_MAX_TURNS: u32 = 50;
const AD_HOC_TIMEOUT_SECS: u64 = 15 * 60;
const LABEL: &str = "claude-code";

pub struct ClaudeCodeTool {
    /// The agent the tool runs for: its logins pay, its ledger is billed.
    service: Arc<AgentService>,
    allowed_paths: Vec<PathBuf>,
}

impl ClaudeCodeTool {
    pub fn new(service: Arc<AgentService>, allowed_paths: Vec<PathBuf>) -> Self {
        Self {
            service,
            allowed_paths,
        }
    }
}

#[derive(Deserialize)]
struct ClaudeCodeInput {
    task: String,
    workdir: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
}

#[async_trait]
impl Tool for ClaudeCodeTool {
    fn name(&self) -> &str {
        "coding.claude"
    }

    fn description(&self) -> &str {
        "Delegate a complex coding task to Claude Code CLI (uses Max subscription). \
         Use for multi-file changes, refactoring, bug fixes, or implementation tasks \
         that benefit from Claude's native coding abilities. Claude Code has its own \
         Read, Edit, Bash, Glob, Grep tools and picks up CLAUDE.md files automatically.\n\n\
         IMPORTANT: If this tool fails due to rate limits or errors, report the error \
         to the user. Do NOT attempt the coding task yourself — it requires Claude Code \
         quality and falling back to API would incur unexpected costs."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Detailed description of the coding task to accomplish"
                },
                "workdir": {
                    "type": "string",
                    "description": "Absolute path to the working directory (repo or worktree)"
                },
                "context": {
                    "type": "string",
                    "description": "Optional additional context (file contents, error logs, design docs)"
                },
                "max_turns": {
                    "type": "integer",
                    "description": "Maximum agentic turns (default: 50)",
                    "default": 50
                },
                "model": {
                    "type": "string",
                    "description": "Claude Code model: opus, sonnet, or haiku (default: subscription default)",
                    "enum": ["opus", "sonnet", "haiku"]
                },
                "effort": {
                    "type": "string",
                    "description": "Thinking effort level: low, medium, high, or max (default: medium)",
                    "enum": ["low", "medium", "high", "max"]
                }
            },
            "required": ["task", "workdir"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;

        let input: ClaudeCodeInput = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid input: {e}"),
                    elapsed(),
                ))
            }
        };

        if let Err(reason) = validate_workdir(&input.workdir, &self.allowed_paths) {
            return Ok(ToolResult::failure(reason, elapsed()));
        }

        let agent_id = self.service.agent_id.as_str();
        let login = match self.service.logins.resolve(SubscriptionKind::Claude) {
            Ok(login) => login,
            Err(no_login) => {
                return Ok(ToolResult::failure(
                    no_login_notice(agent_id, &no_login),
                    elapsed(),
                ))
            }
        };

        let full_task = match input.context {
            Some(ref ctx) if !ctx.is_empty() => {
                format!("## Context\n\n{ctx}\n\n## Task\n\n{}", input.task)
            }
            _ => input.task.clone(),
        };

        let config = ClaudeCodeConfig {
            task: full_task,
            workdir: PathBuf::from(&input.workdir),
            max_turns: Some(input.max_turns.unwrap_or(DEFAULT_MAX_TURNS)),
            allowed_tools: vec![
                "Read".into(),
                "Edit".into(),
                "Write".into(),
                "Bash".into(),
                "Glob".into(),
                "Grep".into(),
            ],
            append_system_prompt: Some(
                "When you finish, provide a clear summary of all changes you made.".into(),
            ),
            timeout: Duration::from_secs(AD_HOC_TIMEOUT_SECS),
            model: input.model,
            effort: input.effort,
            mcp: None,
            session_id: None,
            resume: false,
            persist_session: false,
            load_claude_md: false,
            load_auto_memory: false,
            gh_token: None,
            cache_dir: None,
            remote: None,
            auth: crate::claude_code::ClaudeAuth::Resolved(login.clone()),
            jobs: None,
            contained: None,
        };

        let started_at = chrono::Utc::now();
        let identity = crate::user_config::current_identity_id();
        let ctx = RoundContext {
            service: &self.service,
            identity: identity.as_deref(),
            stand_in: SCHEDULER,
            ticket_id: None,
            description: "coding.claude",
            resume: crate::mcp_gate::RouteResume::Nowhere,
        };
        let run = claude_code::run_with_retry(
            &config,
            self.service.vault.as_deref(),
            self.service.global_vault.as_deref(),
            None,
            None,
        )
        .await
        .inspect(|result| {
            login.settle(&result.round_end(), agent_id);
            ctx.record(
                LABEL,
                &labelled_model(LABEL, result.model.as_deref()),
                result,
                started_at,
                Some(login.served(SubscriptionRoute::ClaudeCode, agent_id)),
            );
        });
        match run {
            Ok(r) if r.exit_code == 0 => Ok(ToolResult::success(
                serde_json::json!({
                    "response": r.text,
                    "duration_secs": r.duration.as_secs(),
                    "turns": r.num_turns,
                    "input_tokens": r.input_tokens,
                    "output_tokens": r.output_tokens,
                    "cost_usd": r.cost_usd,
                }),
                elapsed(),
            )),
            Ok(r) => Ok(ToolResult::failure(
                format!(
                    "Claude Code failed (exit {}). Cannot fall back to API.\n\n{}",
                    r.exit_code, r.text
                ),
                elapsed(),
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Claude Code unavailable: {e}. Cannot fall back to API."),
                elapsed(),
            )),
        }
    }
}
