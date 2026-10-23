//! Email skill — multi-account email integration.
//!
//! Provides tools for reading, searching, sending, and managing email
//! across multiple IMAP/SMTP accounts.

#[cfg(feature = "email")]
pub mod client;
pub mod config;
#[cfg(feature = "email")]
pub mod connector;
pub mod error;
pub mod presets;
#[cfg(feature = "email")]
pub mod setup;
#[cfg(feature = "email")]
pub mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::definition::{
    CredentialRequirement, SkillDefinition, SkillPolicy, SkillRequirements, SkillTool,
};

pub use config::{EmailAccountConfig, EmailSkillConfig};

/// The Email skill — multi-account IMAP/SMTP integration.
///
/// Holds an `ImapConnector` behind `Arc<tokio::sync::Mutex>` for
/// shared access across tools. The connector lazily establishes
/// connections on first use.
pub struct EmailSkill {
    #[cfg(feature = "email")]
    connector: Arc<tokio::sync::Mutex<connector::ImapConnector>>,
    #[allow(dead_code)]
    config: EmailSkillConfig,
    #[cfg(feature = "email")]
    staged: tools::StagedEmailStore,
    #[cfg(feature = "email")]
    contacts: tools::ContactsStore,
    #[cfg(feature = "email")]
    rules: tools::RulesStore,
}

impl EmailSkill {
    /// Create from config. No connections are established yet.
    pub fn new(
        config: EmailSkillConfig,
        #[allow(unused)] workspace: PathBuf,
    ) -> eyre::Result<Self> {
        #[cfg(feature = "email")]
        let connector = Arc::new(tokio::sync::Mutex::new(connector::ImapConnector::new(
            &config,
        )));

        Self::build(config, connector, workspace)
    }

    /// Create with a `SecretStore` for per-user credential resolution.
    pub fn with_store(
        config: EmailSkillConfig,
        #[allow(unused)] workspace: PathBuf,
        #[allow(unused)] store: Arc<dyn workforce_common::settings::SecretStore>,
    ) -> eyre::Result<Self> {
        #[cfg(feature = "email")]
        let connector = Arc::new(tokio::sync::Mutex::new(
            connector::ImapConnector::with_store(&config, store),
        ));

        Self::build(config, connector, workspace)
    }

    fn build(
        config: EmailSkillConfig,
        #[cfg(feature = "email")] connector: Arc<tokio::sync::Mutex<connector::ImapConnector>>,
        #[allow(unused)] workspace: PathBuf,
    ) -> eyre::Result<Self> {
        #[cfg(feature = "email")]
        let staged = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        #[cfg(feature = "email")]
        let contacts = {
            let _ = std::fs::create_dir_all(&workspace);
            let path = workspace.join("contacts.json");
            Arc::new(tokio::sync::Mutex::new(tools::ContactBook::load(path)))
        };

        #[cfg(feature = "email")]
        let rules = {
            let path = workspace.join("rules.json");
            Arc::new(tokio::sync::Mutex::new(tools::RuleBook::load(path)))
        };

        Ok(Self {
            #[cfg(feature = "email")]
            connector,
            config,
            #[cfg(feature = "email")]
            staged,
            #[cfg(feature = "email")]
            contacts,
            #[cfg(feature = "email")]
            rules,
        })
    }

    /// Return all agent-callable tools.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        #[cfg(feature = "email")]
        {
            vec![
                Arc::new(tools::EmailAccountsTool::new(Arc::clone(&self.connector))),
                Arc::new(tools::EmailInboxTool::new(Arc::clone(&self.connector))),
                Arc::new(tools::EmailFoldersTool::new(Arc::clone(&self.connector))),
                Arc::new(tools::EmailReadTool::new(Arc::clone(&self.connector))),
                Arc::new(tools::EmailGetAttachmentTool::new(Arc::clone(
                    &self.connector,
                ))),
                Arc::new(tools::EmailSearchTool::new(Arc::clone(&self.connector))),
                Arc::new(tools::EmailSendTool::new(
                    Arc::clone(&self.connector),
                    Arc::clone(&self.staged),
                    Arc::clone(&self.contacts),
                    Arc::clone(&self.rules),
                )),
                Arc::new(tools::EmailDraftTool::new(Arc::clone(&self.connector))),
                Arc::new(tools::EmailMarkTool::new(Arc::clone(&self.connector))),
                Arc::new(tools::EmailMoveTool::new(Arc::clone(&self.connector))),
                Arc::new(tools::EmailArchiveTool::new(Arc::clone(&self.connector))),
                Arc::new(tools::EmailDeleteTool::new(Arc::clone(&self.connector))),
                Arc::new(tools::EmailForwardTool::new(
                    Arc::clone(&self.connector),
                    Arc::clone(&self.staged),
                    Arc::clone(&self.contacts),
                    Arc::clone(&self.rules),
                )),
                Arc::new(tools::EmailConfirmTool::new(
                    Arc::clone(&self.connector),
                    Arc::clone(&self.staged),
                )),
                Arc::new(tools::EmailContactsTool::new(Arc::clone(&self.contacts))),
                Arc::new(tools::EmailRulesTool::new(Arc::clone(&self.rules))),
            ]
        }

        #[cfg(not(feature = "email"))]
        {
            Vec::new()
        }
    }

    /// Tool names provided by this skill.
    pub fn tool_names() -> Vec<&'static str> {
        vec![
            "email.setup",
            "email.accounts",
            "email.inbox",
            "email.folders",
            "email.read",
            "email.get_attachment",
            "email.search",
            "email.send",
            "email.draft",
            "email.mark",
            "email.move",
            "email.archive",
            "email.delete",
            "email.forward",
            "email.confirm",
            "email.contacts",
            "email.rules",
        ]
    }

    /// Get a reference to the shared IMAP connector.
    #[cfg(feature = "email")]
    pub fn connector(&self) -> Arc<tokio::sync::Mutex<connector::ImapConnector>> {
        Arc::clone(&self.connector)
    }

    /// Skill definition for the Email integration.
    pub fn definition() -> SkillDefinition {
        SkillDefinition {
            name: "email".to_string(),
            version: "1.0.0".to_string(),
            description:
                "Multi-account email integration — read, search, send, and manage email via IMAP/SMTP. Management tools include move, archive, mark, and delete."
                    .to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),

            tools: vec![
                SkillTool {
                    name: "email.setup".to_string(),
                    description: "Email account setup wizard. Actions: list_providers (show presets), test_connection (verify IMAP/SMTP), configure (test + return vault.set commands). Auto-fills server details for Gmail, iCloud, Outlook, ProtonMail, Yahoo, Fastmail.".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "action": {
                                "type": "string",
                                "enum": ["list_providers", "test_connection", "configure"],
                                "description": "Action to perform"
                            },
                            "provider": {
                                "type": "string",
                                "description": "Provider name (gmail, outlook, icloud)"
                            },
                            "email": {
                                "type": "string",
                                "description": "Email address / username"
                            },
                            "password": {
                                "type": "string",
                                "description": "Password or app-specific password"
                            },
                            "imap_host": { "type": "string", "description": "IMAP host (auto-filled from preset)" },
                            "imap_port": { "type": "integer", "description": "IMAP port, integer (default: 993)" },
                            "smtp_host": { "type": "string", "description": "SMTP host (auto-filled from preset)" },
                            "smtp_port": { "type": "integer", "description": "SMTP port, integer (default: 587)" },
                            "tls": { "type": "boolean", "description": "Use TLS (default: true)" },
                            "account_name": { "type": "string", "description": "Account name for vault storage" }
                        },
                        "required": ["action"]
                    }),
                },
                SkillTool {
                    name: "email.accounts".to_string(),
                    description: "List all configured email accounts with connection status".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {}
                    }),
                },
                SkillTool {
                    name: "email.inbox".to_string(),
                    description: "Fetch recent messages from one or all email accounts".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "limit": { "type": "integer" },
                            "folder": { "type": "string" }
                        }
                    }),
                },
                SkillTool {
                    name: "email.folders".to_string(),
                    description: "List all folders/mailboxes for an email account".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" }
                        },
                        "required": ["account"]
                    }),
                },
                SkillTool {
                    name: "email.read".to_string(),
                    description: "Fetch a single email message by UID with full MIME parsing. Its `attachments` list each file's `part`; open one with email.get_attachment.".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "uid": { "type": "integer" },
                            "folder": { "type": "string" }
                        },
                        "required": ["account", "uid"]
                    }),
                },
                SkillTool {
                    name: "email.get_attachment".to_string(),
                    description: "Download an email attachment and render it into your view so you can read it — documents, PDFs, spreadsheets, images, audio or video. Pass the `part` from an email.read `attachments` entry. The file appears on your next step; wait for it before describing the contents.".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": { "type": "string" },
                            "uid": { "type": "integer", "minimum": 1 },
                            "part": { "type": "string" },
                            "folder": { "type": "string" }
                        },
                        "required": ["account", "uid", "part"]
                    }),
                },
                SkillTool {
                    name: "email.search".to_string(),
                    description: "Search for emails across one or all accounts with text, date, sender, and unread filters".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Text to search for in message body and headers"
                            },
                            "account": {
                                "type": "string",
                                "description": "Account name to search. Omit to search all accounts."
                            },
                            "folder": {
                                "type": "string",
                                "description": "Folder to search (default: INBOX)"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum results per account, integer (default: 20, max: 100)"
                            },
                            "since": {
                                "type": "string",
                                "description": "Date filter"
                            },
                            "from": {
                                "type": "string",
                                "description": "Filter by sender email address or name"
                            },
                            "unread_only": {
                                "type": "boolean",
                                "description": "Only return unread messages (default: false)"
                            }
                        }
                    }),
                },
                SkillTool {
                    name: "email.send".to_string(),
                    description: "Send an email from a configured account. Supports plain text and HTML, CC/BCC, and reply threading.".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": {
                                "type": "string",
                                "description": "Account name to send from. Uses default account if omitted."
                            },
                            "to": {
                                "type": "string",
                                "description": "Recipient email address(es), comma-separated for multiple"
                            },
                            "subject": {
                                "type": "string",
                                "description": "Email subject"
                            },
                            "body": {
                                "type": "string",
                                "description": "Plain text body"
                            },
                            "html": {
                                "type": "string",
                                "description": "HTML body"
                            },
                            "cc": {
                                "type": "string",
                                "description": "CC recipients, comma-separated"
                            },
                            "bcc": {
                                "type": "string",
                                "description": "BCC recipients, comma-separated"
                            },
                            "reply_to_uid": {
                                "type": "integer",
                                "description": "UID of message to reply to, integer"
                            },
                            "folder": {
                                "type": "string",
                                "description": "Folder containing the reply-to message (default: INBOX)"
                            }
                        },
                        "required": ["to", "subject", "body"]
                    }),
                },
                SkillTool {
                    name: "email.mark".to_string(),
                    description: "Mark email messages as read/unread or flagged/unflagged. Supports bulk operations via 'uids' array.".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": {
                                "type": "string",
                                "description": "Account name (required)"
                            },
                            "uid": {
                                "type": "integer",
                                "description": "Single message UID (integer — NOT a string)"
                            },
                            "uids": {
                                "type": "array",
                                "items": { "type": "integer" },
                                "description": "Array of message UIDs (integers)"
                            },
                            "folder": {
                                "type": "string",
                                "description": "Folder containing the messages (default: INBOX)"
                            },
                            "action": {
                                "type": "string",
                                "enum": ["read", "unread", "flag", "unflag"],
                                "description": "Action to perform: read, unread, flag, or unflag"
                            }
                        },
                        "required": ["account", "action"]
                    }),
                },
                SkillTool {
                    name: "email.move".to_string(),
                    description: "Move email messages from one folder to another. Supports bulk operations via 'uids' array.".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": {
                                "type": "string",
                                "description": "Account name (required)"
                            },
                            "uid": {
                                "type": "integer",
                                "description": "Single message UID (integer — NOT a string)"
                            },
                            "uids": {
                                "type": "array",
                                "items": { "type": "integer" },
                                "description": "Array of message UIDs (integers)"
                            },
                            "from_folder": {
                                "type": "string",
                                "description": "Source folder (default: INBOX)"
                            },
                            "to_folder": {
                                "type": "string",
                                "description": "Destination folder (required)"
                            }
                        },
                        "required": ["account", "to_folder"]
                    }),
                },
                SkillTool {
                    name: "email.archive".to_string(),
                    description: "Archive email messages. Automatically detects the provider-specific archive folder (e.g. Gmail → All Mail, iCloud → Archive). Supports bulk operations via 'uids' array.".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": {
                                "type": "string",
                                "description": "Account name (required)"
                            },
                            "uid": {
                                "type": "integer",
                                "description": "Single message UID (integer — NOT a string)"
                            },
                            "uids": {
                                "type": "array",
                                "items": { "type": "integer" },
                                "description": "Array of message UIDs (integers)"
                            },
                            "folder": {
                                "type": "string",
                                "description": "Source folder (default: INBOX)"
                            }
                        },
                        "required": ["account"]
                    }),
                },
                SkillTool {
                    name: "email.delete".to_string(),
                    description: "Delete email messages. By default, moves to the provider-specific Trash folder. Set permanent=true for irreversible deletion. Supports bulk operations via 'uids' array.".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": {
                                "type": "string",
                                "description": "Account name (required)"
                            },
                            "uid": {
                                "type": "integer",
                                "description": "Single message UID (integer — NOT a string)"
                            },
                            "uids": {
                                "type": "array",
                                "items": { "type": "integer" },
                                "description": "Array of message UIDs (integers)"
                            },
                            "folder": {
                                "type": "string",
                                "description": "Folder containing the messages (default: INBOX)"
                            },
                            "permanent": {
                                "type": "boolean",
                                "description": "If true, permanently delete (default: false)"
                            }
                        },
                        "required": ["account"]
                    }),
                },
                SkillTool {
                    name: "email.forward".to_string(),
                    description: "Forward an email to new recipients, preserving original sender metadata and attachments. Prefixes subject with 'Fwd:'. Supports forwarding between different configured accounts via send_from.".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "account": {
                                "type": "string",
                                "description": "Account name holding the email to forward (required)"
                            },
                            "uid": {
                                "type": "integer",
                                "description": "UID of the message to forward (integer, required)",
                                "minimum": 1
                            },
                            "folder": {
                                "type": "string",
                                "description": "Folder containing the message to forward (default: INBOX)"
                            },
                            "to": {
                                "type": "string",
                                "description": "Recipient email address(es), comma-separated (required)"
                            },
                            "cc": {
                                "type": "string",
                                "description": "CC recipients, comma-separated"
                            },
                            "bcc": {
                                "type": "string",
                                "description": "BCC recipients, comma-separated"
                            },
                            "comment": {
                                "type": "string",
                                "description": "Optional comment to add above the forwarded content"
                            },
                            "include_attachments": {
                                "type": "boolean",
                                "description": "Include original attachments (default: true)"
                            },
                            "send_from": {
                                "type": "string",
                                "description": "Account name to send from (defaults to 'account')"
                            }
                        },
                        "required": ["account", "uid", "to"]
                    }),
                },
                SkillTool {
                    name: "email.confirm".to_string(),
                    description: "Confirm or cancel a staged email. After email.send or email.forward stages an email, use this to send or discard it.".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "confirmation_id": {
                                "type": "string",
                                "description": "The confirmation_id returned by email.send or email.forward"
                            },
                            "action": {
                                "type": "string",
                                "enum": ["send", "cancel"],
                                "description": "Action to perform (default: send)"
                            }
                        },
                        "required": ["confirmation_id"]
                    }),
                },
                SkillTool {
                    name: "email.contacts".to_string(),
                    description: "Manage the email address book. Actions: add (name + email), list, search (query), remove (email).".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "action": {
                                "type": "string",
                                "enum": ["add", "list", "search", "remove"],
                                "description": "Action to perform"
                            },
                            "name": {
                                "type": "string",
                                "description": "Contact name (for add)"
                            },
                            "email": {
                                "type": "string",
                                "description": "Contact email address (for add/remove)"
                            },
                            "query": {
                                "type": "string",
                                "description": "Search query (for search)"
                            }
                        },
                        "required": ["action"]
                    }),
                },
                SkillTool {
                    name: "email.rules".to_string(),
                    description: "Manage pre-approval rules for auto-sending emails. Actions: add (to + optional subject_pattern), list, remove (id).".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "action": {
                                "type": "string",
                                "enum": ["add", "list", "remove"],
                                "description": "Action to perform"
                            },
                            "id": {
                                "type": "string",
                                "description": "Rule ID (for add/remove)"
                            },
                            "to": {
                                "type": "string",
                                "description": "Recipient pattern to match (for add)"
                            },
                            "subject_pattern": {
                                "type": "string",
                                "description": "Optional regex pattern for subject matching (for add)"
                            },
                            "enabled": {
                                "type": "boolean",
                                "description": "Whether the rule is enabled (default: true, for add)"
                            }
                        },
                        "required": ["action"]
                    }),
                },
            ],
            event_sources: Vec::new(),
            default_policies: vec![
                SkillPolicy {
                    tool: "email.setup".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "email.accounts".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "email.inbox".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "email.folders".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "email.read".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "email.get_attachment".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "email.search".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "email.send".to_string(),
                    action: "require_approval".to_string(),
                },
                SkillPolicy {
                    tool: "email.mark".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "email.move".to_string(),
                    action: "require_approval".to_string(),
                },
                SkillPolicy {
                    tool: "email.archive".to_string(),
                    action: "require_approval".to_string(),
                },
                SkillPolicy {
                    tool: "email.delete".to_string(),
                    action: "require_approval".to_string(),
                },
                SkillPolicy {
                    tool: "email.forward".to_string(),
                    action: "require_approval".to_string(),
                },
                SkillPolicy {
                    tool: "email.confirm".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "email.contacts".to_string(),
                    action: "allow".to_string(),
                },
                SkillPolicy {
                    tool: "email.rules".to_string(),
                    action: "require_approval".to_string(),
                },
            ],
            memory_schemas: Vec::new(),

            requirements: SkillRequirements {
                credentials: vec![
                    CredentialRequirement {
                        name: "username".to_string(),
                        r#type: "string".to_string(),
                        description: "Email account username / email address".to_string(),
                    },
                    CredentialRequirement {
                        name: "password".to_string(),
                        r#type: "string".to_string(),
                        description: "Email account password or app-specific password".to_string(),
                    },
                ],
                config: Vec::new(),
            },

            dependencies: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_attachment_is_named_defined_and_allowed() {
        let definition = EmailSkill::definition();
        assert!(EmailSkill::tool_names().contains(&"email.get_attachment"));
        assert!(definition
            .tools
            .iter()
            .any(|t| t.name == "email.get_attachment"));
        assert!(definition
            .default_policies
            .iter()
            .any(|p| p.tool == "email.get_attachment" && p.action == "allow"));
    }

    #[cfg(feature = "email")]
    #[test]
    fn every_registered_tool_is_named() {
        let workspace = tempfile::tempdir().unwrap();
        let config = EmailSkillConfig {
            accounts: std::collections::HashMap::new(),
            default_account: None,
        };
        let skill = EmailSkill::new(config, workspace.path().to_path_buf()).unwrap();
        let names = EmailSkill::tool_names();
        let tools = skill.tools();
        assert!(tools.iter().any(|t| t.name() == "email.get_attachment"));
        for tool in tools {
            assert!(names.contains(&tool.name()), "{} is not named", tool.name());
        }
    }
}
