//! Agent-callable tool for inter-agent messaging.
//!
//! Allows agents to discover peers, send messages, and check their inbox.
//! Supports audit logging and configurable oversight modes (DW-219).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};

use crate::agent_comms_audit::{AgentCommsAuditLog, CommStatus, OversightConfig, OversightMode};
use crate::agent_inbox::{AgentInbox, AgentMessage};
use crate::hosts::RegistryView;

/// Tool for inter-agent messaging.
pub struct AgentMessageTool {
    registry: Arc<dyn RegistryView>,
    inbox: Arc<AgentInbox>,
    agent_id: String,
    audit_log: Option<Arc<AgentCommsAuditLog>>,
    oversight: OversightConfig,
}

impl AgentMessageTool {
    pub fn new(registry: Arc<dyn RegistryView>, inbox: Arc<AgentInbox>, agent_id: String) -> Self {
        Self {
            registry,
            inbox,
            agent_id,
            audit_log: None,
            oversight: OversightConfig::default(),
        }
    }

    pub fn new_with_oversight(
        registry: Arc<dyn RegistryView>,
        inbox: Arc<AgentInbox>,
        agent_id: String,
        audit_log: Arc<AgentCommsAuditLog>,
        oversight: OversightConfig,
    ) -> Self {
        Self {
            registry,
            inbox,
            agent_id,
            audit_log: Some(audit_log),
            oversight,
        }
    }
}

#[derive(Deserialize)]
struct MessageInput {
    action: String,
    #[serde(default)]
    to_agent: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    message_type: Option<String>,
    #[serde(default)]
    correlation_id: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for AgentMessageTool {
    fn name(&self) -> &str {
        "agent_message"
    }

    fn description(&self) -> &str {
        "Send and receive messages between agents"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list_agents", "send", "check_inbox", "audit"],
                    "description": "Action to perform"
                },
                "to_agent": {
                    "type": "string",
                    "description": "Target agent ID (for send)"
                },
                "content": {
                    "type": "string",
                    "description": "Message content (for send)"
                },
                "message_type": {
                    "type": "string",
                    "description": "Message type (for send, e.g. 'request', 'info', 'response')"
                },
                "correlation_id": {
                    "type": "string",
                    "description": "Optional correlation ID for request/response tracking"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max messages to return (for check_inbox/audit, default 20)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: MessageInput = serde_json::from_value(input)?;

        let result = match input.action.as_str() {
            "list_agents" => {
                let agents = self.registry.agent_ids().into_iter().filter_map(|id| {
                    let service = self.registry.get(id.as_str())?;
                    Some(serde_json::json!({ "id": id.as_str(), "capabilities": service.tools.names() }))
                }).collect::<Vec<_>>();
                serde_json::json!({ "agents": agents })
            }
            "send" => {
                let to = input
                    .to_agent
                    .ok_or_else(|| eyre::eyre!("to_agent is required"))?;
                let content = input
                    .content
                    .ok_or_else(|| eyre::eyre!("content is required"))?;

                // Verify target agent exists
                if self.registry.get(&to).is_none() {
                    return Ok(ToolResult {
                        success: false,
                        output: serde_json::json!({ "error": format!("Unknown agent: {to}") }),
                        error: Some(format!("Unknown agent: {to}")),
                        rate_limited: None,
                        duration_ms: start.elapsed().as_millis() as u64,
                        loads: Vec::new(),
                    });
                }

                // Keep a copy of content for audit before moving into the message
                let content_str = content.clone();

                let msg = AgentMessage {
                    from_agent: self.agent_id.clone(),
                    to_agent: to.clone(),
                    content,
                    message_type: input.message_type.unwrap_or_else(|| "info".to_string()),
                    timestamp: Utc::now(),
                    correlation_id: input.correlation_id,
                };

                // Check oversight mode
                if let Some(ref audit) = self.audit_log {
                    let is_exempt = self.oversight.is_pair_auto_approved(&self.agent_id, &to);

                    if is_exempt {
                        // Auto-approve exempt pairs regardless of mode
                        let delivered = self.inbox.push(msg).await;
                        let status = if delivered {
                            CommStatus::AutoApproved
                        } else {
                            CommStatus::Failed("Inbox full".to_string())
                        };
                        let record = AgentCommsAuditLog::make_record(
                            &self.agent_id,
                            &to,
                            &content_str,
                            status,
                        );
                        audit.record(record).await;
                        serde_json::json!({
                            "delivered": delivered,
                            "to_agent": to,
                            "note": if delivered { "Message auto-approved and delivered" } else { "Inbox full, message dropped" }
                        })
                    } else {
                        match self.oversight.mode {
                            OversightMode::Auto => {
                                let delivered = self.inbox.push(msg).await;
                                let status = if delivered {
                                    CommStatus::AutoApproved
                                } else {
                                    CommStatus::Failed("Inbox full".to_string())
                                };
                                let record = AgentCommsAuditLog::make_record(
                                    &self.agent_id,
                                    &to,
                                    &content_str,
                                    status,
                                );
                                audit.record(record).await;
                                serde_json::json!({
                                    "delivered": delivered,
                                    "to_agent": to,
                                    "note": if delivered { "Message auto-approved and delivered" } else { "Inbox full, message dropped" }
                                })
                            }
                            OversightMode::RequireApproval => {
                                // Hold for approval — do not deliver
                                let record = AgentCommsAuditLog::make_record(
                                    &self.agent_id,
                                    &to,
                                    &content_str,
                                    CommStatus::Pending,
                                );
                                let record_id = record.id.clone();
                                audit.record(record).await;
                                serde_json::json!({
                                    "delivered": false,
                                    "to_agent": to,
                                    "awaiting_approval": true,
                                    "record_id": record_id,
                                    "note": "Message held for owner approval"
                                })
                            }
                            OversightMode::Notify => {
                                let delivered = self.inbox.push(msg).await;
                                let status = if delivered {
                                    CommStatus::Delivered
                                } else {
                                    CommStatus::Failed("Inbox full".to_string())
                                };
                                let record = AgentCommsAuditLog::make_record(
                                    &self.agent_id,
                                    &to,
                                    &content_str,
                                    status,
                                );
                                audit.record(record).await;
                                serde_json::json!({
                                    "delivered": delivered,
                                    "to_agent": to,
                                    "owner_notified": true,
                                    "note": if delivered { "Message delivered, owner notified" } else { "Inbox full, message dropped" }
                                })
                            }
                        }
                    }
                } else {
                    // No audit log configured — deliver directly (backward compat)
                    let delivered = self.inbox.push(msg).await;
                    serde_json::json!({
                        "delivered": delivered,
                        "to_agent": to,
                        "note": if delivered { "Message queued for delivery" } else { "Inbox full, message dropped" }
                    })
                }
            }
            "check_inbox" => {
                let limit = input.limit.unwrap_or(20);
                let messages = self.inbox.drain(&self.agent_id).await;
                let count = messages.len();
                let display: Vec<_> = messages
                    .into_iter()
                    .take(limit)
                    .map(|m| {
                        serde_json::json!({
                            "from": m.from_agent,
                            "type": m.message_type,
                            "content": m.content,
                            "timestamp": m.timestamp.to_rfc3339(),
                            "correlation_id": m.correlation_id,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "count": count,
                    "messages": display
                })
            }
            "audit" => {
                if let Some(ref audit) = self.audit_log {
                    let limit = input.limit.unwrap_or(20);
                    let records = audit.list(limit).await;
                    serde_json::json!({
                        "count": records.len(),
                        "records": records.iter().map(|r| {
                            serde_json::json!({
                                "id": r.id,
                                "timestamp": r.timestamp.to_rfc3339(),
                                "from": r.from_agent,
                                "to": r.to_agent,
                                "preview": r.content_preview,
                                "status": r.status,
                            })
                        }).collect::<Vec<_>>()
                    })
                } else {
                    serde_json::json!({
                        "count": 0,
                        "records": [],
                        "note": "Audit logging is not configured"
                    })
                }
            }
            other => {
                return Ok(ToolResult {
                    success: false,
                    output: serde_json::json!({ "error": format!("Unknown action: {other}") }),
                    error: Some(format!(
                        "Unknown action: {other}. Use list_agents, send, check_inbox, or audit"
                    )),
                    rate_limited: None,
                    duration_ms: start.elapsed().as_millis() as u64,
                    loads: Vec::new(),
                });
            }
        };

        Ok(ToolResult {
            success: true,
            output: result,
            error: None,
            rate_limited: None,
            duration_ms: start.elapsed().as_millis() as u64,
            loads: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_core::types::AgentId;

    use crate::hosts::{RoutedApproval, Unloaded};
    use crate::serve::AgentService;
    use crate::vault::Vault;

    /// A registry with no agents: these tests exercise the empty listing and
    /// the "unknown agent" path, which need no real services.
    struct NoAgents;

    impl RegistryView for NoAgents {
        fn get(&self, _id: &str) -> Option<Arc<AgentService>> {
            None
        }
        fn default_agent(&self) -> Option<Arc<AgentService>> {
            None
        }
        fn agent_ids(&self) -> Vec<AgentId> {
            Vec::new()
        }
        fn all_services(&self) -> Vec<Arc<AgentService>> {
            Vec::new()
        }
        fn unloaded(&self) -> Vec<Unloaded> {
            Vec::new()
        }
        fn admits(&self, _agent: &str, _identity: &str) -> bool {
            false
        }
        fn login_vaults(&self, _global: Option<&Arc<Vault>>) -> Vec<Arc<Vault>> {
            Vec::new()
        }
        fn routed_delivery(self: Arc<Self>) -> tokio::sync::mpsc::Sender<RoutedApproval> {
            tokio::sync::mpsc::channel(1).0
        }
    }

    #[tokio::test]
    async fn list_agents_returns_empty_registry() {
        let registry = Arc::new(NoAgents);
        let inbox = Arc::new(AgentInbox::new());
        let tool = AgentMessageTool::new(registry, inbox, "agent-a".to_string());

        let result = tool
            .execute(serde_json::json!({ "action": "list_agents" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["agents"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn send_to_unknown_agent_fails_gracefully() {
        let registry = Arc::new(NoAgents);
        let inbox = Arc::new(AgentInbox::new());
        let tool = AgentMessageTool::new(registry, inbox, "agent-a".to_string());

        let result = tool
            .execute(serde_json::json!({
                "action": "send",
                "to_agent": "nonexistent",
                "content": "hello"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown agent"));
    }

    #[tokio::test]
    async fn check_inbox_returns_and_drains_messages() {
        let registry = Arc::new(NoAgents);
        let inbox = Arc::new(AgentInbox::new());

        // Manually push a message into the inbox
        inbox
            .push(AgentMessage {
                from_agent: "agent-b".to_string(),
                to_agent: "agent-a".to_string(),
                content: "hi from b".to_string(),
                message_type: "info".to_string(),
                timestamp: Utc::now(),
                correlation_id: None,
            })
            .await;

        let tool = AgentMessageTool::new(registry, Arc::clone(&inbox), "agent-a".to_string());

        // First check should return the message
        let result = tool
            .execute(serde_json::json!({ "action": "check_inbox" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"], 1);
        let msgs = result.output["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["from"], "agent-b");
        assert_eq!(msgs[0]["content"], "hi from b");

        // Second check should be empty (messages were drained)
        let result = tool
            .execute(serde_json::json!({ "action": "check_inbox" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"], 0);
    }

    #[tokio::test]
    async fn unknown_action_fails() {
        let registry = Arc::new(NoAgents);
        let inbox = Arc::new(AgentInbox::new());
        let tool = AgentMessageTool::new(registry, inbox, "agent-a".to_string());

        let result = tool
            .execute(serde_json::json!({ "action": "bogus" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown action"));
    }

    #[tokio::test]
    async fn audit_without_log_returns_empty() {
        let registry = Arc::new(NoAgents);
        let inbox = Arc::new(AgentInbox::new());
        let tool = AgentMessageTool::new(registry, inbox, "agent-a".to_string());

        let result = tool
            .execute(serde_json::json!({ "action": "audit" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"], 0);
        assert!(result.output["note"]
            .as_str()
            .unwrap()
            .contains("not configured"));
    }
}
