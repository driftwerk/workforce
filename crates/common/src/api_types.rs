//! Standardized API data models for the management HTTP endpoints (DW-425).
//!
//! These types provide a consistent, type-safe contract for request and response
//! bodies across the config, policy, engagement, and memory API endpoints.
//! Using strongly-typed structs instead of ad-hoc `serde_json::Value` construction
//! ensures serialization consistency and makes the API self-documenting.

use serde::{Deserialize, Serialize};

// ── Configuration ─────────────────────────────────────────────────────────

/// Response body for `GET /api/config/schema`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSchemaResponse {
    pub sections: Vec<ConfigSection>,
}

/// Describes one configuration section (e.g. "llm", "compaction") and its fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSection {
    pub name: String,
    pub title: String,
    pub description: String,
    pub fields: Vec<ConfigField>,
}

/// A single configurable field within a section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigField {
    pub key: String,
    pub label: String,
    pub description: String,
    pub field_type: ConfigFieldType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_value: Option<serde_json::Value>,
    #[serde(default)]
    pub required: bool,
    /// For "select" fields: the list of valid options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
    /// Validation constraints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation: Option<FieldValidation>,
}

/// The data type of a config field, used to select the appropriate form control.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFieldType {
    String,
    Integer,
    Float,
    Boolean,
    Select,
    StringArray,
    KeyValueMap,
}

/// Validation constraints for a config field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldValidation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

/// Response body for `PUT /api/config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigUpdateResponse {
    pub updated: bool,
    pub message: String,
}

// ── Policy ────────────────────────────────────────────────────────────────

/// Response body for `GET /api/policy/rules`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRulesResponse {
    pub rules: Vec<PolicyRuleEntry>,
    pub total: usize,
    pub constitutional: usize,
}

/// A single policy rule as returned by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRuleEntry {
    pub name: String,
    pub scope: serde_json::Value,
    pub tool: String,
    pub action: String,
    pub priority: i32,
    pub constitutional: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conditions: Option<serde_json::Value>,
}

/// Response body for `POST /api/policy/rules`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRuleCreatedResponse {
    pub added: String,
    pub total_rules: usize,
}

/// Response body for `PUT /api/policy/rules/{name}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRuleUpdatedResponse {
    pub updated: String,
}

/// Response body for `DELETE /api/policy/rules/{name}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRuleDeletedResponse {
    pub deleted: String,
}

/// Request body for `POST /api/policy/proposals`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyProposalCreateRequest {
    #[serde(default)]
    pub agent_id: Option<String>,
    pub proposed_rule: String,
    pub justification: String,
}

/// Response body for `GET /api/policy/proposals`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyProposalsResponse {
    pub proposals: Vec<PolicyProposalEntry>,
    pub count: usize,
}

/// A single policy proposal as returned by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyProposalEntry {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub proposed_rule: String,
    pub justification: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_note: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_at: Option<String>,
}

/// Response body for `POST /api/policy/proposals`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyProposalCreatedResponse {
    pub id: i64,
    pub status: String,
}

/// Request body for `POST /api/policy/proposals/{id}/approve` and `reject`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyProposalReviewRequest {
    #[serde(default)]
    pub reviewed_by: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Response body for proposal approve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyProposalApprovedResponse {
    pub approved: i64,
}

/// Response body for proposal reject.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyProposalRejectedResponse {
    pub rejected: i64,
}

// ── Engagement ────────────────────────────────────────────────────────────

/// Response body for `GET /api/engagement/modes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngagementModesResponse {
    pub overrides: Vec<EngagementModeEntry>,
    pub count: usize,
}

/// Identifies the agent + channel (+ optional thread) an override applies to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngagementModeRef {
    pub agent_id: String,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// A single engagement mode override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngagementModeEntry {
    pub agent_id: String,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub mode: String,
    pub set_by: String,
    pub set_at: String,
}

/// Request body for `PUT /api/engagement/modes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngagementModeSetRequest {
    pub agent_id: String,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub mode: String,
}

/// Response body for `PUT /api/engagement/modes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngagementModeSetResponse {
    pub agent_id: String,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    pub mode: String,
}

/// Request body for `DELETE /api/engagement/modes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngagementModeDeleteRequest {
    pub agent_id: String,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// Response body for `DELETE /api/engagement/modes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngagementModeDeletedResponse {
    pub removed: EngagementModeRef,
}

// ── Memory ────────────────────────────────────────────────────────────────

/// Response body for `GET /api/agents/{id}/memory/files`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFilesResponse {
    pub agent_id: String,
    pub files: Vec<MemoryFileEntry>,
    pub count: usize,
}

/// A single memory file in the listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFileEntry {
    pub path: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
}

/// Response body for `GET /api/agents/{id}/memory/files/{path}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFileReadResponse {
    pub agent_id: String,
    pub path: String,
    pub content: String,
    pub size: u64,
}

/// Request body for `PUT /api/agents/{id}/memory/files/{path}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFileWriteRequest {
    pub content: String,
}

/// Response body for `PUT /api/agents/{id}/memory/files/{path}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFileWriteResponse {
    pub written: String,
    pub size: usize,
}

/// Response body for `GET /api/agents/{id}/memory/tasks`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryTasksResponse {
    pub agent_id: String,
    pub tasks: Vec<serde_json::Value>,
    pub count: usize,
}

/// Response body for `GET /api/agents/{id}/memory/stats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStatsResponse {
    pub agent_id: String,
    pub file_count: usize,
    pub total_size_bytes: u64,
    pub daily_note_count: usize,
    pub task_session_count: usize,
}

// ── Error envelope ────────────────────────────────────────────────────────

/// Standardized JSON error response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorResponse {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// ── Schema builders ───────────────────────────────────────────────────────

impl ConfigSchemaResponse {
    /// Build the full configuration schema with all editable sections.
    pub fn build() -> Self {
        Self {
            sections: vec![
                Self::llm_section(),
                Self::compaction_section(),
                Self::gateway_section(),
                Self::heartbeat_section(),
            ],
        }
    }

    fn llm_section() -> ConfigSection {
        ConfigSection {
            name: "llm".to_string(),
            title: "LLM Configuration".to_string(),
            description: "Language model provider settings, model selection, and token limits."
                .to_string(),
            fields: vec![
                ConfigField {
                    key: "provider".to_string(),
                    label: "Provider".to_string(),
                    description: "LLM provider type.".to_string(),
                    field_type: ConfigFieldType::Select,
                    default_value: Some(serde_json::json!("anthropic")),
                    required: true,
                    options: Some(vec![
                        "anthropic".to_string(),
                        "openrouter".to_string(),
                        "chatgpt".to_string(),
                        "claude-code".to_string(),
                        "codex".to_string(),
                    ]),
                    validation: None,
                },
                ConfigField {
                    key: "default_model".to_string(),
                    label: "Default Model".to_string(),
                    description: "The default model identifier for LLM calls.".to_string(),
                    field_type: ConfigFieldType::String,
                    default_value: Some(serde_json::json!("claude-sonnet-4-20250514")),
                    required: true,
                    options: None,
                    validation: None,
                },
                ConfigField {
                    key: "max_tokens".to_string(),
                    label: "Max Tokens".to_string(),
                    description: "Maximum tokens per LLM response.".to_string(),
                    field_type: ConfigFieldType::Integer,
                    default_value: Some(serde_json::json!(16384)),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: Some(1.0),
                        max: Some(128000.0),
                        pattern: None,
                    }),
                },
                ConfigField {
                    key: "max_rounds".to_string(),
                    label: "Max Rounds".to_string(),
                    description: "Maximum tool execution rounds per message.".to_string(),
                    field_type: ConfigFieldType::Integer,
                    default_value: Some(serde_json::json!(50)),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: Some(1.0),
                        max: Some(500.0),
                        pattern: None,
                    }),
                },
                ConfigField {
                    key: "failover".to_string(),
                    label: "Failover Chain".to_string(),
                    description: "Ordered list of fallback models when the primary fails."
                        .to_string(),
                    field_type: ConfigFieldType::StringArray,
                    default_value: Some(serde_json::json!([])),
                    required: false,
                    options: None,
                    validation: None,
                },
                ConfigField {
                    key: "aliases".to_string(),
                    label: "Model Aliases".to_string(),
                    description: "Friendly name → full model identifier mappings.".to_string(),
                    field_type: ConfigFieldType::KeyValueMap,
                    default_value: Some(serde_json::json!({})),
                    required: false,
                    options: None,
                    validation: None,
                },
                ConfigField {
                    key: "thinking_budget_tokens".to_string(),
                    label: "Thinking Budget".to_string(),
                    description: "Extended thinking token budget (optional).".to_string(),
                    field_type: ConfigFieldType::Integer,
                    default_value: None,
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: Some(1024.0),
                        max: Some(128000.0),
                        pattern: None,
                    }),
                },
                ConfigField {
                    key: "model_max_tokens".to_string(),
                    label: "Per-Model Max Tokens".to_string(),
                    description: "Model-specific max_tokens overrides.".to_string(),
                    field_type: ConfigFieldType::KeyValueMap,
                    default_value: Some(serde_json::json!({})),
                    required: false,
                    options: None,
                    validation: None,
                },
            ],
        }
    }

    fn compaction_section() -> ConfigSection {
        ConfigSection {
            name: "compaction".to_string(),
            title: "Context Compaction".to_string(),
            description: "Controls how the context window is managed and compacted.".to_string(),
            fields: vec![
                ConfigField {
                    key: "max_context_tokens".to_string(),
                    label: "Max Context Tokens".to_string(),
                    description: "Maximum context window size in tokens.".to_string(),
                    field_type: ConfigFieldType::Integer,
                    default_value: Some(serde_json::json!(200000)),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: Some(1000.0),
                        max: Some(1000000.0),
                        pattern: None,
                    }),
                },
                ConfigField {
                    key: "compaction_threshold".to_string(),
                    label: "Compaction Threshold".to_string(),
                    description:
                        "Fraction of max tokens at which compaction triggers (0.0 to 1.0)."
                            .to_string(),
                    field_type: ConfigFieldType::Float,
                    default_value: Some(serde_json::json!(0.8)),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: Some(0.1),
                        max: Some(1.0),
                        pattern: None,
                    }),
                },
                ConfigField {
                    key: "keep_recent_messages".to_string(),
                    label: "Keep Recent Messages".to_string(),
                    description: "Number of recent messages to always keep during compaction."
                        .to_string(),
                    field_type: ConfigFieldType::Integer,
                    default_value: Some(serde_json::json!(20)),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: Some(1.0),
                        max: Some(200.0),
                        pattern: None,
                    }),
                },
                ConfigField {
                    key: "enable_memory_flush".to_string(),
                    label: "Memory Flush".to_string(),
                    description: "Flush important context to memory files before compacting."
                        .to_string(),
                    field_type: ConfigFieldType::Boolean,
                    default_value: Some(serde_json::json!(true)),
                    required: false,
                    options: None,
                    validation: None,
                },
                ConfigField {
                    key: "prune_tool_outputs".to_string(),
                    label: "Prune Tool Outputs".to_string(),
                    description:
                        "Truncate large tool outputs as a lighter alternative to full compaction."
                            .to_string(),
                    field_type: ConfigFieldType::Boolean,
                    default_value: Some(serde_json::json!(true)),
                    required: false,
                    options: None,
                    validation: None,
                },
                ConfigField {
                    key: "prune_tool_output_max_chars".to_string(),
                    label: "Max Tool Output Chars".to_string(),
                    description: "Maximum character length for tool output before truncation."
                        .to_string(),
                    field_type: ConfigFieldType::Integer,
                    default_value: Some(serde_json::json!(2000)),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: Some(100.0),
                        max: Some(100000.0),
                        pattern: None,
                    }),
                },
                ConfigField {
                    key: "max_compaction_passes".to_string(),
                    label: "Max Compaction Passes".to_string(),
                    description: "Maximum compaction iterations per cycle (safety valve)."
                        .to_string(),
                    field_type: ConfigFieldType::Integer,
                    default_value: Some(serde_json::json!(3)),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: Some(1.0),
                        max: Some(10.0),
                        pattern: None,
                    }),
                },
                ConfigField {
                    key: "min_messages_to_compact".to_string(),
                    label: "Min Messages to Compact".to_string(),
                    description: "Minimum messages required before compaction runs.".to_string(),
                    field_type: ConfigFieldType::Integer,
                    default_value: Some(serde_json::json!(5)),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: Some(1.0),
                        max: Some(100.0),
                        pattern: None,
                    }),
                },
                ConfigField {
                    key: "urgent_compaction_threshold".to_string(),
                    label: "Urgent Compaction Threshold".to_string(),
                    description: "Fraction at which compaction blocks synchronously (0.0 to 1.0)."
                        .to_string(),
                    field_type: ConfigFieldType::Float,
                    default_value: Some(serde_json::json!(0.95)),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: Some(0.5),
                        max: Some(1.0),
                        pattern: None,
                    }),
                },
                ConfigField {
                    key: "compaction_model".to_string(),
                    label: "Compaction Model".to_string(),
                    description:
                        "Model for compaction LLM calls (optional, defaults to agent model)."
                            .to_string(),
                    field_type: ConfigFieldType::String,
                    default_value: None,
                    required: false,
                    options: None,
                    validation: None,
                },
            ],
        }
    }

    fn gateway_section() -> ConfigSection {
        ConfigSection {
            name: "gateway".to_string(),
            title: "Gateway".to_string(),
            description: "HTTP gateway listen address and authentication.".to_string(),
            fields: vec![
                ConfigField {
                    key: "listen".to_string(),
                    label: "Listen Address".to_string(),
                    description: "Gateway listen address (host:port).".to_string(),
                    field_type: ConfigFieldType::String,
                    default_value: Some(serde_json::json!("0.0.0.0:3000")),
                    required: false,
                    options: None,
                    validation: None,
                },
                ConfigField {
                    key: "cors".to_string(),
                    label: "CORS Enabled".to_string(),
                    description: "Enable CORS headers for web clients.".to_string(),
                    field_type: ConfigFieldType::Boolean,
                    default_value: Some(serde_json::json!(true)),
                    required: false,
                    options: None,
                    validation: None,
                },
            ],
        }
    }

    fn heartbeat_section() -> ConfigSection {
        ConfigSection {
            name: "heartbeat".to_string(),
            title: "Heartbeat".to_string(),
            description: "Proactive heartbeat check scheduling and delivery.".to_string(),
            fields: vec![
                ConfigField {
                    key: "enabled".to_string(),
                    label: "Enabled".to_string(),
                    description: "Whether heartbeat is enabled.".to_string(),
                    field_type: ConfigFieldType::Boolean,
                    default_value: Some(serde_json::json!(false)),
                    required: false,
                    options: None,
                    validation: None,
                },
                ConfigField {
                    key: "interval_secs".to_string(),
                    label: "Interval (seconds)".to_string(),
                    description: "Seconds between heartbeat cycles.".to_string(),
                    field_type: ConfigFieldType::Integer,
                    default_value: Some(serde_json::json!(1800)),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: Some(60.0),
                        max: Some(86400.0),
                        pattern: None,
                    }),
                },
                ConfigField {
                    key: "active_start".to_string(),
                    label: "Active Start".to_string(),
                    description: "Active hours start (HH:MM, user timezone).".to_string(),
                    field_type: ConfigFieldType::String,
                    default_value: Some(serde_json::json!("08:00")),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: None,
                        max: None,
                        pattern: Some(r"^\d{2}:\d{2}$".to_string()),
                    }),
                },
                ConfigField {
                    key: "active_end".to_string(),
                    label: "Active End".to_string(),
                    description: "Active hours end (HH:MM, user timezone).".to_string(),
                    field_type: ConfigFieldType::String,
                    default_value: Some(serde_json::json!("23:00")),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: None,
                        max: None,
                        pattern: Some(r"^\d{2}:\d{2}$".to_string()),
                    }),
                },
                ConfigField {
                    key: "channels".to_string(),
                    label: "Delivery Channels".to_string(),
                    description: "Channels to deliver heartbeat output to.".to_string(),
                    field_type: ConfigFieldType::StringArray,
                    default_value: Some(serde_json::json!([])),
                    required: false,
                    options: None,
                    validation: None,
                },
                ConfigField {
                    key: "deliver".to_string(),
                    label: "Delivery Mode".to_string(),
                    description: "Which heartbeat responses to deliver.".to_string(),
                    field_type: ConfigFieldType::Select,
                    default_value: Some(serde_json::json!("actionable_only")),
                    required: false,
                    options: Some(vec!["all".to_string(), "actionable_only".to_string()]),
                    validation: None,
                },
                ConfigField {
                    key: "max_checks_per_cycle".to_string(),
                    label: "Max Checks per Cycle".to_string(),
                    description: "Maximum heartbeat checks to run per cycle.".to_string(),
                    field_type: ConfigFieldType::Integer,
                    default_value: Some(serde_json::json!(5)),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: Some(1.0),
                        max: Some(50.0),
                        pattern: None,
                    }),
                },
                ConfigField {
                    key: "dedup_window_secs".to_string(),
                    label: "Dedup Window (seconds)".to_string(),
                    description: "Suppress re-notification of same issue within this window."
                        .to_string(),
                    field_type: ConfigFieldType::Integer,
                    default_value: Some(serde_json::json!(3600)),
                    required: false,
                    options: None,
                    validation: Some(FieldValidation {
                        min: Some(0.0),
                        max: Some(86400.0),
                        pattern: None,
                    }),
                },
                ConfigField {
                    key: "log_to_daily_notes".to_string(),
                    label: "Log to Daily Notes".to_string(),
                    description: "Log heartbeat findings to daily notes.".to_string(),
                    field_type: ConfigFieldType::Boolean,
                    default_value: Some(serde_json::json!(false)),
                    required: false,
                    options: None,
                    validation: None,
                },
                ConfigField {
                    key: "skip_during_active_sessions".to_string(),
                    label: "Skip During Active Sessions".to_string(),
                    description: "Skip heartbeats when a user session is active.".to_string(),
                    field_type: ConfigFieldType::Boolean,
                    default_value: Some(serde_json::json!(false)),
                    required: false,
                    options: None,
                    validation: None,
                },
            ],
        }
    }
}
