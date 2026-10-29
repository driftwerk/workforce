//! Email agent-callable tools.
//!
//! Implements IMAP-based reading tools, SMTP-based send/forward tools,
//! and IMAP-based management tools:
//! - `email.accounts` — list configured accounts with connection status
//! - `email.inbox` — fetch recent messages from one or all accounts
//! - `email.folders` — list folders/mailboxes for an account
//! - `email.read` — fetch a single message by UID with full MIME parsing
//! - `email.search` — unified cross-account search with text, date, sender, and unread filters
//! - `email.send` — send and reply to emails via SMTP with TLS
//! - `email.forward` — forward an existing email preserving metadata and attachments
//! - `email.confirm` — confirm or cancel a staged email
//! - `email.contacts` — manage the address book
//! - `email.rules` — manage pre-approval rules
//! - `email.mark` — mark messages as read/unread or flagged/unflagged
//! - `email.move` — move messages between folders
//! - `email.archive` — archive messages with provider-specific folder detection
//! - `email.delete` — delete messages (move to trash or permanent)

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_imap::types::Fetch;
use async_trait::async_trait;
use eyre::Result;
use futures::TryStreamExt;
use mailparse::MailHeaderMap;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use workforce_agent::security::external_content::wrap_external_content;
use workforce_agent::tools::image_view::{
    stage_for_view, view_pending_note, within_view_cap, MAX_VIEW_BYTES, VIEWED_IMAGES_KEY,
};
use workforce_agent::tools::{Tool, ToolResult};

use super::connector::ImapConnector;

use crate::adapters::outbound_mail::{assemble, MailPart};
use crate::adapters::utils::{
    artifact_attachments_schema, coerce_to_id_string, coerce_to_u64, resolve_artifact_attachments,
};

// ── Shared state types ──────────────────────────────────────────────

pub type StagedEmailStore = Arc<tokio::sync::Mutex<HashMap<String, StagedEmail>>>;
pub type ContactsStore = Arc<tokio::sync::Mutex<ContactBook>>;
pub type RulesStore = Arc<tokio::sync::Mutex<RuleBook>>;

pub struct StagedEmail {
    pub message: lettre::Message,
    pub raw_message: Vec<u8>,
    pub account_name: String,
    pub config: super::config::EmailAccountConfig,
    pub preview: EmailPreview,
    pub staged_at: chrono::DateTime<chrono::Utc>,
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailPreview {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_preview: String,
    pub unknown_recipients: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub name: String,
    pub email: String,
}

pub struct ContactBook {
    contacts: Vec<Contact>,
    path: PathBuf,
}

impl ContactBook {
    pub fn load(path: PathBuf) -> Self {
        let contacts = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { contacts, path }
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::write(&self.path, serde_json::to_string_pretty(&self.contacts)?)
    }

    pub fn add(&mut self, contact: Contact) {
        if !self
            .contacts
            .iter()
            .any(|c| c.email.eq_ignore_ascii_case(&contact.email))
        {
            self.contacts.push(contact);
            let _ = self.save();
        }
    }

    pub fn remove(&mut self, email: &str) -> bool {
        let before = self.contacts.len();
        self.contacts
            .retain(|c| !c.email.eq_ignore_ascii_case(email));
        if self.contacts.len() < before {
            let _ = self.save();
            true
        } else {
            false
        }
    }

    pub fn search(&self, query: &str) -> Vec<&Contact> {
        let q = query.to_lowercase();
        self.contacts
            .iter()
            .filter(|c| c.name.to_lowercase().contains(&q) || c.email.to_lowercase().contains(&q))
            .collect()
    }

    pub fn list(&self) -> &[Contact] {
        &self.contacts
    }

    pub fn contains_email(&self, email: &str) -> bool {
        self.contacts
            .iter()
            .any(|c| c.email.eq_ignore_ascii_case(email))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreApprovalRule {
    pub id: String,
    pub to: String,
    pub subject_pattern: Option<String>,
    pub enabled: bool,
}

pub struct RuleBook {
    rules: Vec<PreApprovalRule>,
    path: PathBuf,
}

impl RuleBook {
    pub fn load(path: PathBuf) -> Self {
        let rules = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { rules, path }
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::write(&self.path, serde_json::to_string_pretty(&self.rules)?)
    }

    pub fn add(&mut self, rule: PreApprovalRule) {
        self.rules.retain(|r| r.id != rule.id);
        self.rules.push(rule);
        let _ = self.save();
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.rules.len();
        self.rules.retain(|r| r.id != id);
        if self.rules.len() < before {
            let _ = self.save();
            true
        } else {
            false
        }
    }

    pub fn list(&self) -> &[PreApprovalRule] {
        &self.rules
    }

    pub fn matches(&self, to: &str, subject: &str) -> bool {
        self.rules.iter().any(|r| {
            if !r.enabled {
                return false;
            }
            if !to.to_lowercase().contains(&r.to.to_lowercase()) {
                return false;
            }
            match &r.subject_pattern {
                Some(pattern) => regex::Regex::new(pattern)
                    .map(|re| re.is_match(subject))
                    .unwrap_or(false),
                None => true,
            }
        })
    }
}

// ── Security scanning helpers ───────────────────────────────────────

const SENSITIVE_KEYWORDS: &[&str] = &[
    "password",
    "confidential",
    "private",
    "secret",
    "invoice",
    "receipt",
    "ssn",
    "credit card",
    "bank account",
    "routing number",
    "social security",
];

fn scan_sensitive_content(subject: &str, body: &str) -> Vec<String> {
    let combined = format!("{} {}", subject, body).to_lowercase();
    SENSITIVE_KEYWORDS
        .iter()
        .filter(|kw| combined.contains(**kw))
        .map(|kw| format!("sensitive_content: {kw}"))
        .collect()
}

fn check_unknown_recipients(emails: &[String], contacts: &ContactBook) -> Vec<String> {
    emails
        .iter()
        .filter(|e| !contacts.contains_email(e))
        .cloned()
        .collect()
}

// ── Helpers ──────────────────────────────────────────────────────────

fn markdown_to_html(markdown: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let parser = Parser::new_ext(markdown, Options::all());
    let mut output = String::new();
    html::push_html(&mut output, parser);
    output
}

/// Infer a friendly provider name from the IMAP host.
fn infer_provider(imap_host: &str) -> &str {
    let host = imap_host.to_lowercase();
    if host.contains("gmail") || host.contains("google") {
        "Gmail"
    } else if host.contains("outlook") || host.contains("office365") || host.contains("hotmail") {
        "Outlook"
    } else if host.contains("yahoo") {
        "Yahoo"
    } else if host.contains("icloud") || host.contains("apple") || host.contains("me.com") {
        "iCloud"
    } else if host.contains("aol") {
        "AOL"
    } else if host.contains("zoho") {
        "Zoho"
    } else if host.contains("fastmail") {
        "Fastmail"
    } else if host.contains("proton") || host.contains("protonmail") {
        "ProtonMail"
    } else {
        "IMAP"
    }
}

/// Map common IMAP folder names to friendly display names.
fn friendly_folder_name(name: &str) -> String {
    let mappings: &[(&str, &str)] = &[
        ("[Gmail]/All Mail", "All Mail"),
        ("[Gmail]/Drafts", "Drafts"),
        ("[Gmail]/Important", "Important"),
        ("[Gmail]/Sent Mail", "Sent"),
        ("[Gmail]/Spam", "Spam"),
        ("[Gmail]/Starred", "Starred"),
        ("[Gmail]/Trash", "Trash"),
        ("[Gmail]", "[Gmail]"),
        ("Sent Items", "Sent"),
        ("Deleted Items", "Trash"),
        ("Junk Email", "Spam"),
        ("Junk", "Spam"),
        ("Bulk Mail", "Spam"),
        ("Sent Messages", "Sent"),
        ("INBOX", "Inbox"),
        ("Drafts", "Drafts"),
        ("Trash", "Trash"),
        ("Spam", "Spam"),
        ("Sent", "Sent"),
        ("Archive", "Archive"),
    ];

    for &(pattern, friendly) in mappings {
        if name.eq_ignore_ascii_case(pattern) {
            return friendly.to_string();
        }
    }

    if let Some(stripped) = name.strip_prefix("[Gmail]/") {
        return stripped.to_string();
    }
    if let Some(stripped) = name.strip_prefix("INBOX.") {
        return stripped.to_string();
    }
    if let Some(stripped) = name.strip_prefix("INBOX/") {
        return stripped.to_string();
    }

    name.to_string()
}

/// Extract a plain-text preview from raw text, truncated to ~100 chars.
fn text_preview(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.len() <= max_chars {
        trimmed.to_string()
    } else {
        let mut end = max_chars;
        if let Some(pos) = trimmed[..max_chars].rfind(' ') {
            if pos > max_chars / 2 {
                end = pos;
            }
        }
        format!("{}…", &trimmed[..end])
    }
}

/// Format a byte size as human-readable.
fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Decode a Cow<[u8]> to a UTF-8 string.
fn cow_to_string(cow: &[u8]) -> String {
    String::from_utf8_lossy(cow).to_string()
}

/// Check if flags indicate the message is unread (no \Seen flag).
fn is_unread_flags(flags: &[String]) -> bool {
    !flags.iter().any(|f| f.contains("Seen"))
}

/// Format an address from IMAP envelope fields.
fn format_single_address(
    name: &Option<Cow<'_, [u8]>>,
    mailbox: &Option<Cow<'_, [u8]>>,
    host: &Option<Cow<'_, [u8]>>,
) -> String {
    let name_str = name.as_ref().map(|n| cow_to_string(n)).unwrap_or_default();
    let mbox_str = mailbox
        .as_ref()
        .map(|m| cow_to_string(m))
        .unwrap_or_default();
    let host_str = host.as_ref().map(|h| cow_to_string(h)).unwrap_or_default();

    let email = if !mbox_str.is_empty() && !host_str.is_empty() {
        format!("{mbox_str}@{host_str}")
    } else {
        mbox_str
    };

    if !name_str.is_empty() {
        format!("{name_str} <{email}>")
    } else {
        email
    }
}

/// Maximum body length before truncation.
const MAX_BODY_CHARS: usize = 50_000;

/// Truncate body text at MAX_BODY_CHARS with a note about total length.
fn truncate_body(text: &str) -> String {
    if text.len() <= MAX_BODY_CHARS {
        text.to_string()
    } else {
        let total = text.len();
        let truncated = &text[..MAX_BODY_CHARS];
        format!(
            "{truncated}\n\n[truncated — full message is {total} chars, showing first {MAX_BODY_CHARS}]"
        )
    }
}

// ── email.accounts ───────────────────────────────────────────────────

pub struct EmailAccountsTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
}

impl EmailAccountsTool {
    pub fn new(connector: Arc<tokio::sync::Mutex<ImapConnector>>) -> Self {
        Self { connector }
    }
}

#[async_trait]
impl Tool for EmailAccountsTool {
    fn name(&self) -> &str {
        "email.accounts"
    }

    fn description(&self) -> &str {
        "List all configured email accounts with connection status"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let mut connector = self.connector.lock().await;
        let account_names = connector.resolve_account_names();
        let default_account = connector.default_account().to_string();

        let mut accounts = Vec::new();

        for name in &account_names {
            // Resolve each account under the CALLER's scope, never the baked
            // (loading-user) snapshot: on a shared agent the old `configs()`
            // leaked another user's account name + username (email) here.
            let Some(config) = connector.resolve_account_config(name) else {
                continue;
            };
            let provider = infer_provider(&config.imap_host);
            let display_name = config.display_name.clone().unwrap_or_default();
            let is_default = name == &default_account;

            // Attempt connection health check
            let status = match connector.get(Some(name)).await {
                Ok(client) => {
                    if client.is_connected() {
                        "connected"
                    } else {
                        "disconnected"
                    }
                }
                Err(e) => {
                    warn!(account = %name, error = %e, "Account health check failed");
                    accounts.push(serde_json::json!({
                        "account": name,
                        "username": config.username,
                        "display_name": display_name,
                        "provider": provider,
                        "imap_host": config.imap_host,
                        "status": "error",
                        "error": e.to_string(),
                        "default": is_default,
                    }));
                    continue;
                }
            };

            accounts.push(serde_json::json!({
                "account": name,
                "username": config.username,
                "display_name": display_name,
                "provider": provider,
                "imap_host": config.imap_host,
                "status": status,
                "default": is_default,
            }));
        }

        // Sort with default account first
        accounts.sort_by(|a, b| {
            let a_def = a["default"].as_bool().unwrap_or(false);
            let b_def = b["default"].as_bool().unwrap_or(false);
            b_def.cmp(&a_def)
        });

        let output = serde_json::json!({
            "accounts": accounts,
            "total": accounts.len(),
        });

        Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ── email.inbox ──────────────────────────────────────────────────────

pub struct EmailInboxTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
}

impl EmailInboxTool {
    pub fn new(connector: Arc<tokio::sync::Mutex<ImapConnector>>) -> Self {
        Self { connector }
    }
}

#[async_trait]
impl Tool for EmailInboxTool {
    fn name(&self) -> &str {
        "email.inbox"
    }

    fn description(&self) -> &str {
        "Fetch recent messages from one or all email accounts"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "account": {
                    "type": "string",
                    "description": "Account name to fetch from. Omit to fetch from all accounts."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of messages (integer, default: 20, max: 100)",
                    "minimum": 1,
                    "maximum": 100
                },
                "folder": {
                    "type": "string",
                    "description": "Folder to fetch from (default: INBOX)"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let account_filter = input["account"].as_str();
        let limit = coerce_to_u64(&input["limit"]).unwrap_or(20).min(100) as usize;
        let folder = input["folder"].as_str().unwrap_or("INBOX");

        let mut connector = self.connector.lock().await;

        // Determine which accounts to fetch from (caller-scoped, not the baked
        // loading-user snapshot — see `resolve_account_names`).
        let account_names: Vec<String> = if let Some(acct) = account_filter {
            vec![acct.to_string()]
        } else {
            connector.resolve_account_names()
        };

        let mut all_messages = Vec::new();
        let mut errors = Vec::new();

        // Sequential fetch across accounts (single Mutex prevents true parallelism)
        for acct_name in &account_names {
            match fetch_inbox_for_account(&mut connector, acct_name, folder, limit).await {
                Ok(msgs) => all_messages.extend(msgs),
                Err(e) => {
                    warn!(account = %acct_name, error = %e, "Failed to fetch inbox");
                    errors.push(serde_json::json!({
                        "account": acct_name,
                        "error": e.to_string(),
                    }));
                }
            }
        }

        // Messages come back in ascending UID order; reverse for newest first
        all_messages.reverse();
        all_messages.truncate(limit);

        let output = serde_json::json!({
            "messages": all_messages,
            "count": all_messages.len(),
            "folder": folder,
            "errors": errors,
        });

        Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        ))
    }
}

/// Fetch inbox messages for a single account.
async fn fetch_inbox_for_account(
    connector: &mut ImapConnector,
    account: &str,
    folder: &str,
    limit: usize,
) -> std::result::Result<Vec<serde_json::Value>, super::error::EmailError> {
    let client = connector.get(Some(account)).await?;
    let session = client.session().await?;

    // Use EXAMINE for read-only access
    let mailbox = session
        .examine(folder)
        .await
        .map_err(|e| super::error::EmailError::Imap(format!("Failed to examine {folder}: {e}")))?;

    let exists = mailbox.exists;
    if exists == 0 {
        return Ok(Vec::new());
    }

    // Fetch the most recent N messages by sequence number
    let start_seq = if exists > limit as u32 {
        exists - limit as u32 + 1
    } else {
        1
    };
    let range = format!("{start_seq}:*");

    debug!(account, folder, range = %range, "Fetching inbox messages");

    let fetches: Vec<_> = session
        .fetch(&range, "(UID FLAGS ENVELOPE BODY.PEEK[TEXT]<0.200>)")
        .await
        .map_err(|e| super::error::EmailError::Imap(format!("Failed to fetch messages: {e}")))?
        .try_collect()
        .await
        .map_err(|e| {
            super::error::EmailError::Imap(format!("Failed to collect fetch results: {e}"))
        })?;

    let mut messages = Vec::new();
    for fetch in &fetches {
        let uid = fetch.uid.unwrap_or(0);
        let flags: Vec<String> = fetch.flags().map(|f| format!("{f:?}")).collect();
        let unread = is_unread_flags(&flags);

        let (date, from, subject) = if let Some(env) = fetch.envelope() {
            let date_str = env
                .date
                .as_ref()
                .map(|d| cow_to_string(d))
                .unwrap_or_default();
            let from_str = env
                .from
                .as_ref()
                .and_then(|addrs| addrs.first())
                .map(|addr| format_single_address(&addr.name, &addr.mailbox, &addr.host))
                .unwrap_or_default();
            let subj_str = env
                .subject
                .as_ref()
                .map(|s| cow_to_string(s))
                .unwrap_or_else(|| "(no subject)".to_string());
            (date_str, from_str, subj_str)
        } else {
            (String::new(), String::new(), "(no envelope)".to_string())
        };

        let preview = fetch
            .text()
            .map(|t| {
                let text_str = String::from_utf8_lossy(t);
                text_preview(&text_str, 100)
            })
            .unwrap_or_default();

        messages.push(serde_json::json!({
            "uid": uid,
            "account": account,
            "sender": from,
            "subject": subject,
            "date": date,
            "preview": preview,
            "unread": unread,
            "folder": folder,
        }));
    }

    Ok(messages)
}

// ── email.folders ────────────────────────────────────────────────────

pub struct EmailFoldersTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
}

impl EmailFoldersTool {
    pub fn new(connector: Arc<tokio::sync::Mutex<ImapConnector>>) -> Self {
        Self { connector }
    }
}

#[async_trait]
impl Tool for EmailFoldersTool {
    fn name(&self) -> &str {
        "email.folders"
    }

    fn description(&self) -> &str {
        "List all folders/mailboxes for an email account with message counts and"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "account": {
                    "type": "string",
                    "description": "Account name to list folders for (required)"
                }
            },
            "required": ["account"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let account = match input["account"].as_str() {
            Some(a) => a,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'account'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let mut connector = self.connector.lock().await;
        let client = match connector.get(Some(account)).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let session = match client.session().await {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // LIST all mailboxes — collect the stream into a Vec
        let mailboxes: Vec<_> = session
            .list(Some(""), Some("*"))
            .await
            .map_err(|e| eyre::eyre!("Failed to list folders: {e}"))?
            .try_collect()
            .await
            .map_err(|e| eyre::eyre!("Failed to collect folder list: {e}"))?;

        let mut folders = Vec::new();

        for mailbox in &mailboxes {
            let name = mailbox.name().to_string();
            let friendly = friendly_folder_name(&name);

            // Try to get STATUS for message counts
            let (messages, unseen) = match session.status(&name, "(MESSAGES UNSEEN)").await {
                Ok(status) => {
                    let msgs = status.exists;
                    let unsn = status.unseen.unwrap_or(0);
                    (msgs, unsn)
                }
                Err(e) => {
                    debug!(folder = %name, error = %e, "STATUS failed for folder");
                    (0, 0)
                }
            };

            folders.push(serde_json::json!({
                "name": name,
                "friendly_name": friendly,
                "messages": messages,
                "unread": unseen,
            }));
        }

        let output = serde_json::json!({
            "account": account,
            "folders": folders,
            "total": folders.len(),
        });

        Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ── email.read ───────────────────────────────────────────────────────

pub struct EmailReadTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
}

impl EmailReadTool {
    pub fn new(connector: Arc<tokio::sync::Mutex<ImapConnector>>) -> Self {
        Self { connector }
    }
}

#[async_trait]
impl Tool for EmailReadTool {
    fn name(&self) -> &str {
        "email.read"
    }

    fn description(&self) -> &str {
        "Fetch a single email message by UID. Its `attachments` list each file's `part`; open one \
         with email.get_attachment."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "account": {
                    "type": "string",
                    "description": "Account name (required)"
                },
                "uid": {
                    "type": "integer",
                    "description": "Message UID (integer, not string)",
                    "minimum": 1
                },
                "folder": {
                    "type": "string",
                    "description": "Folder containing the message (default: INBOX)"
                }
            },
            "required": ["account", "uid"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let MessageLocator {
            account,
            uid,
            folder,
        } = match message_locator(&input) {
            Ok(l) => l,
            Err(msg) => return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64)),
        };

        let mut connector = self.connector.lock().await;
        let fetch = match fetch_message(
            &mut connector,
            account,
            folder,
            uid,
            "(FLAGS ENVELOPE BODY[])",
        )
        .await
        {
            Ok(f) => f,
            Err(e) => {
                return Ok(ToolResult::from_report(
                    &e,
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        // Extract envelope data
        let (from, to, cc, date, subject) = if let Some(env) = fetch.envelope() {
            let from_str = format_address_list(env.from.as_deref());
            let to_str = format_address_list(env.to.as_deref());
            let cc_str = format_address_list(env.cc.as_deref());
            let date_str = env
                .date
                .as_ref()
                .map(|d| cow_to_string(d))
                .unwrap_or_default();
            let subj_str = env
                .subject
                .as_ref()
                .map(|s| cow_to_string(s))
                .unwrap_or_else(|| "(no subject)".to_string());

            (from_str, to_str, cc_str, date_str, subj_str)
        } else {
            (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                "(no envelope)".to_string(),
            )
        };

        // Parse flags
        let flags: Vec<String> = fetch.flags().map(|f| format!("{f:?}")).collect();

        // Parse MIME body using mailparse
        let raw_body = fetch.body().unwrap_or(b"");
        let (body_text, body_markdown, attachments) = parse_mime_body(raw_body);

        // Wrap body content for prompt injection protection
        let wrapped_text = if body_text.is_empty() {
            String::new()
        } else {
            wrap_external_content(&body_text, &format!("email from {from}")).wrapped_content
        };

        let wrapped_markdown = if body_markdown.is_empty() {
            String::new()
        } else {
            wrap_external_content(&body_markdown, &format!("email from {from}")).wrapped_content
        };

        let output = serde_json::json!({
            "uid": uid,
            "account": account,
            "folder": folder,
            "from": from,
            "to": to,
            "cc": cc,
            "date": date,
            "subject": subject,
            "flags": flags,
            "unread": is_unread_flags(&flags),
            "body_text": wrapped_text,
            "body_markdown": wrapped_markdown,
            "attachments": attachments,
        });

        Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ── email.get_attachment ─────────────────────────────────────────────

pub struct EmailGetAttachmentTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
}

impl EmailGetAttachmentTool {
    pub fn new(connector: Arc<tokio::sync::Mutex<ImapConnector>>) -> Self {
        Self { connector }
    }
}

#[async_trait]
impl Tool for EmailGetAttachmentTool {
    fn name(&self) -> &str {
        "email.get_attachment"
    }

    fn description(&self) -> &str {
        "Download an email attachment and render it into your view so you can read it — \
         documents, PDFs, spreadsheets, images, audio or video. Pass the `part` from an email.read \
         `attachments` entry. The file appears on your next step; wait for it before describing \
         the contents."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "account": {
                    "type": "string",
                    "description": "Account name (required)"
                },
                "uid": {
                    "type": "integer",
                    "description": "Message UID (integer, not string)",
                    "minimum": 1
                },
                "part": {
                    "type": "string",
                    "description": "The `part` of an email.read `attachments` entry, e.g. '2' or '1.2'"
                },
                "folder": {
                    "type": "string",
                    "description": "Folder containing the message (default: INBOX)"
                }
            },
            "required": ["account", "uid", "part"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;

        let MessageLocator {
            account,
            uid,
            folder,
        } = match message_locator(&input) {
            Ok(l) => l,
            Err(msg) => return Ok(ToolResult::failure(msg, elapsed())),
        };
        let Some(part) = coerce_to_id_string(&input["part"]).filter(|p| !p.is_empty()) else {
            return Ok(ToolResult::failure(
                "Missing required parameter 'part' (an attachments[].part from email.read)",
                elapsed(),
            ));
        };

        let fetch = {
            let mut connector = self.connector.lock().await;
            fetch_message(&mut connector, account, folder, uid, "BODY.PEEK[]").await
        };
        let fetch = match fetch {
            Ok(f) => f,
            Err(e) => return Ok(ToolResult::from_report(&e, elapsed())),
        };
        let mail = match mailparse::parse_mail(fetch.body().unwrap_or(b"")) {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to parse message UID {uid}: {e}"),
                    elapsed(),
                ))
            }
        };
        let file = match select_part(walk_mime(&mail).parts, &part) {
            Ok(f) => f,
            Err(msg) => return Ok(ToolResult::failure(msg, elapsed())),
        };
        if !within_view_cap(file.data.len() as u64) {
            return Ok(ToolResult::failure(
                format!(
                    "Attachment `{}` is {} bytes, over the {MAX_VIEW_BYTES}-byte limit",
                    file.filename,
                    file.data.len()
                ),
                elapsed(),
            ));
        }

        let staged = match stage_for_view(
            "email.get_attachment",
            file.data,
            &file.mime_type,
            &file.filename,
        ) {
            Ok(s) => s,
            Err(msg) => return Ok(ToolResult::failure(msg, elapsed())),
        };

        info!(account = %account, uid, part = %part, filename = %file.filename, "Staged email attachment for viewing");
        Ok(ToolResult::success(
            serde_json::json!({
                VIEWED_IMAGES_KEY: staged.marker,
                "note": view_pending_note(&file.filename),
            }),
            elapsed(),
        ))
    }
}

/// The part whose walker path is `part`, or a message naming the parts the
/// message does have.
fn select_part(mut parts: Vec<MimePart>, part: &str) -> Result<MimePart, String> {
    match parts.iter().position(|p| p.part == part) {
        Some(index) => Ok(parts.swap_remove(index)),
        None if parts.is_empty() => Err(format!(
            "Part '{part}' not found — this message has no attachments"
        )),
        None => Err(format!(
            "Part '{part}' not found — available parts: {}",
            parts
                .iter()
                .map(|p| format!("{} ({})", p.part, p.filename))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The message a tool call addresses: `account` and `uid` are required,
/// `folder` defaults to INBOX.
struct MessageLocator<'a> {
    account: &'a str,
    uid: u32,
    folder: &'a str,
}

fn message_locator(input: &serde_json::Value) -> Result<MessageLocator<'_>, &'static str> {
    let account = input["account"]
        .as_str()
        .filter(|a| !a.trim().is_empty())
        .ok_or("Missing required parameter 'account'")?;
    let uid = coerce_to_u64(&input["uid"])
        .and_then(|u| u32::try_from(u).ok())
        .filter(|u| *u > 0)
        .ok_or("Missing required parameter 'uid' (must be a positive integer)")?;
    Ok(MessageLocator {
        account,
        uid,
        folder: input["folder"].as_str().unwrap_or("INBOX"),
    })
}

/// Fetch one message by UID with the given fetch items, opening `folder`
/// read-only (EXAMINE).
async fn fetch_message(
    connector: &mut ImapConnector,
    account: &str,
    folder: &str,
    uid: u32,
    items: &str,
) -> Result<Fetch> {
    let session = connector.get(Some(account)).await?.session().await?;
    session
        .examine(folder)
        .await
        .map_err(|e| eyre::eyre!("Failed to examine folder {folder}: {e}"))?;
    let fetches: Vec<Fetch> = session
        .uid_fetch(uid.to_string(), items)
        .await
        .map_err(|e| eyre::eyre!("Failed to fetch UID {uid}: {e}"))?
        .try_collect()
        .await
        .map_err(|e| eyre::eyre!("Failed to collect fetch results: {e}"))?;
    fetches
        .into_iter()
        .next()
        .ok_or_else(|| eyre::eyre!("Message UID {uid} not found in {folder}"))
}

/// Format an IMAP address list (from imap_proto types) into a comma-separated string.
fn format_address_list(addrs: Option<&[async_imap::imap_proto::types::Address<'_>]>) -> String {
    match addrs {
        Some(list) => list
            .iter()
            .map(|addr| format_single_address(&addr.name, &addr.mailbox, &addr.host))
            .collect::<Vec<_>>()
            .join(", "),
        None => String::new(),
    }
}

/// Parse a raw email body using mailparse and extract text, HTML→markdown, and attachments.
fn parse_mime_body(raw: &[u8]) -> (String, String, Vec<serde_json::Value>) {
    let mail = match mailparse::parse_mail(raw) {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "Failed to parse MIME body");
            let text = String::from_utf8_lossy(raw).to_string();
            return (truncate_body(&text), String::new(), Vec::new());
        }
    };

    let content = walk_mime(&mail);
    let markdown = if content.html.is_empty() {
        String::new()
    } else {
        htmd::convert(&content.html).unwrap_or_else(|_| content.html.clone())
    };

    (
        truncate_body(&content.text),
        truncate_body(&markdown),
        content.parts.iter().map(attachment_summary).collect(),
    )
}

/// The agent-facing listing entry for a MIME part; `part` is what
/// `email.get_attachment` takes to open it.
fn attachment_summary(part: &MimePart) -> serde_json::Value {
    let mut summary = serde_json::json!({
        "part": part.part,
        "filename": part.filename,
        "mime_type": part.mime_type,
        "size": format_size(part.data.len()),
        "size_bytes": part.data.len(),
    });
    if part.inline {
        summary["inline"] = serde_json::Value::Bool(true);
    }
    summary
}

/// A file carried by a message: an attachment-disposition node, or an inline
/// non-text leaf. `part` is its walker path — the root is `"0"`, the root's
/// children `"1"`, `"2"`, grandchildren `"2.1"` — deterministic for the same
/// raw message and resolved only by re-walking it. It is NOT an IMAP
/// `BODY[section]` number.
struct MimePart {
    part: String,
    filename: String,
    mime_type: String,
    inline: bool,
    data: Vec<u8>,
}

/// Everything readable in a message: its text and HTML bodies (each joined
/// with a blank line) and its files in walk order.
#[derive(Default)]
struct MimeContent {
    text: String,
    html: String,
    parts: Vec<MimePart>,
}

impl MimeContent {
    fn merge(mut self, other: Self) -> Self {
        self.text = join_bodies(self.text, other.text);
        self.html = join_bodies(self.html, other.html);
        self.parts.extend(other.parts);
        self
    }
}

fn join_bodies(first: String, second: String) -> String {
    match (first.is_empty(), second.is_empty()) {
        (_, true) => first,
        (true, false) => second,
        (false, false) => format!("{first}\n\n{second}"),
    }
}

fn walk_mime(mail: &mailparse::ParsedMail) -> MimeContent {
    walk_mime_node(mail, "0")
}

fn walk_mime_node(node: &mailparse::ParsedMail, path: &str) -> MimeContent {
    let disposition = node.get_content_disposition();
    let file = |inline| MimePart {
        part: path.to_string(),
        filename: part_filename(node, &disposition),
        mime_type: node.ctype.mimetype.clone(),
        inline,
        data: node.get_body_raw().unwrap_or_default(),
    };
    if disposition.disposition == mailparse::DispositionType::Attachment {
        return MimeContent {
            parts: vec![file(false)],
            ..MimeContent::default()
        };
    }
    if !node.subparts.is_empty() {
        return node
            .subparts
            .iter()
            .enumerate()
            .map(|(i, sub)| walk_mime_node(sub, &child_path(path, i + 1)))
            .fold(MimeContent::default(), MimeContent::merge);
    }
    match node.ctype.mimetype.as_str() {
        "text/plain" => MimeContent {
            text: node.get_body().unwrap_or_default(),
            ..MimeContent::default()
        },
        "text/html" => MimeContent {
            html: node.get_body().unwrap_or_default(),
            ..MimeContent::default()
        },
        _ => MimeContent {
            parts: Some(file(true))
                .filter(|p| !p.data.is_empty())
                .into_iter()
                .collect(),
            ..MimeContent::default()
        },
    }
}

fn child_path(parent: &str, index: usize) -> String {
    match parent {
        "0" => index.to_string(),
        _ => format!("{parent}.{index}"),
    }
}

/// The part's filename: the disposition `filename`, then the content-type
/// `name`, then `"unnamed"`.
fn part_filename(
    node: &mailparse::ParsedMail,
    disposition: &mailparse::ParsedContentDisposition,
) -> String {
    header_param(node, "Content-Disposition", &disposition.params, "filename")
        .or_else(|| header_param(node, "Content-Type", &node.ctype.params, "name"))
        .unwrap_or_else(|| "unnamed".to_string())
}

/// A header parameter's value. mailparse splits parameters on every `;`, even
/// inside quotes, so the quoted form is read with a quote-aware scan of the
/// (RFC 2047-decoded) header; bare tokens and RFC 2231 `key*=` forms come from
/// mailparse's decoded `params`.
fn header_param(
    node: &mailparse::ParsedMail,
    header: &str,
    params: &std::collections::BTreeMap<String, String>,
    key: &str,
) -> Option<String> {
    node.headers
        .get_first_value(header)
        .and_then(|raw| quoted_param(&raw, key))
        .or_else(|| params.get(key).cloned())
        .filter(|v| !v.is_empty())
}

/// Read the quoted value of `key` from a raw parameterised header value,
/// honouring `\` escapes and `;` inside the quotes.
fn quoted_param(header: &str, key: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let needle = format!("{key}=\"");
    let start = lower
        .match_indices(&needle)
        .find(|(i, _)| lower[..*i].trim_end().ends_with(';'))
        .map(|(i, _)| i + needle.len())?;
    let mut chars = header[start..].chars();
    let mut value = String::new();
    loop {
        match chars.next()? {
            '"' => return Some(value),
            '\\' => value.push(chars.next()?),
            c => value.push(c),
        }
    }
}

// ── email.search helpers ─────────────────────────────────────────────

/// Parse a `since` parameter into an IMAP-formatted date string (`dd-Mon-yyyy`).
///
/// Accepts two formats:
/// - Relative: `"7d"`, `"30d"`, `"1d"` — days before now
/// - ISO date: `"2026-01-15"` — absolute date
///
/// Returns the IMAP date string or an error message.
fn parse_since_date(since: &str) -> Result<String, String> {
    let trimmed = since.trim();

    // Try relative format: e.g. "7d", "30d"
    if let Some(days_str) = trimmed.strip_suffix('d') {
        if let Ok(days) = days_str.parse::<i64>() {
            if days <= 0 {
                return Err(format!(
                    "Invalid relative date '{since}': days must be positive"
                ));
            }
            let date = chrono::Utc::now().date_naive() - chrono::Duration::days(days);
            return Ok(date.format("%d-%b-%Y").to_string());
        }
    }

    // Try ISO date format: "2026-01-15"
    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        return Ok(date.format("%d-%b-%Y").to_string());
    }

    Err(format!(
        "Invalid date format '{since}'. Use relative (e.g. '7d', '30d') or ISO date (e.g. '2026-01-15')."
    ))
}

/// Build an IMAP SEARCH criteria string from the given filters.
///
/// Multiple criteria are space-separated (IMAP AND logic).
/// Returns an error if no criteria at all are provided.
fn build_search_criteria(
    query: Option<&str>,
    since: Option<&str>,
    from: Option<&str>,
    unread_only: bool,
) -> Result<String, String> {
    let mut parts = Vec::new();

    if let Some(q) = query {
        let q = q.trim();
        if !q.is_empty() {
            parts.push(format!("TEXT \"{}\"", q.replace('"', "\\\"")));
        }
    }

    if let Some(s) = since {
        let imap_date = parse_since_date(s)?;
        parts.push(format!("SINCE {imap_date}"));
    }

    if let Some(f) = from {
        let f = f.trim();
        if !f.is_empty() {
            parts.push(format!("FROM \"{}\"", f.replace('"', "\\\"")));
        }
    }

    if unread_only {
        parts.push("UNSEEN".to_string());
    }

    if parts.is_empty() {
        return Err(
            "No search criteria provided. Specify at least a query, date filter, sender filter, or unread_only."
                .to_string(),
        );
    }

    Ok(parts.join(" "))
}

// ── email.search ─────────────────────────────────────────────────────

/// Search a single account across the specified folders.
///
/// Follows the same pattern as `fetch_inbox_for_account()`:
/// EXAMINE (read-only) → uid_search → uid_fetch metadata for matches.
async fn search_account(
    connector: &mut ImapConnector,
    account: &str,
    folders: &[String],
    criteria: &str,
    limit: usize,
) -> std::result::Result<Vec<serde_json::Value>, super::error::EmailError> {
    let client = connector.get(Some(account)).await?;
    let session = client.session().await?;

    let mut all_results = Vec::new();

    for folder in folders {
        // Use EXAMINE for read-only access — if a folder fails, skip it
        let mailbox = match session.examine(folder.as_str()).await {
            Ok(m) => m,
            Err(e) => {
                warn!(
                    account,
                    folder = %folder,
                    error = %e,
                    "Failed to examine folder for search, skipping"
                );
                continue;
            }
        };

        if mailbox.exists == 0 {
            continue;
        }

        // UID SEARCH with our criteria
        let uids: Vec<u32> = match session.uid_search(criteria).await {
            Ok(uid_set) => {
                let mut v: Vec<u32> = uid_set.into_iter().collect();
                v.sort_unstable();
                v
            }
            Err(e) => {
                warn!(
                    account,
                    folder = %folder,
                    error = %e,
                    "IMAP SEARCH failed, skipping folder"
                );
                continue;
            }
        };

        if uids.is_empty() {
            continue;
        }

        // Take the last `limit` UIDs (most recent by UID order)
        let selected_uids: Vec<u32> = if uids.len() > limit {
            uids[uids.len() - limit..].to_vec()
        } else {
            uids
        };

        // Build UID range string for FETCH
        let uid_range = selected_uids
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");

        debug!(
            account,
            folder = %folder,
            uid_count = selected_uids.len(),
            "Fetching search result metadata"
        );

        let fetches: Vec<_> = match session
            .uid_fetch(&uid_range, "(UID FLAGS ENVELOPE BODY.PEEK[TEXT]<0.200>)")
            .await
        {
            Ok(stream) => match stream.try_collect().await {
                Ok(f) => f,
                Err(e) => {
                    warn!(
                        account,
                        folder = %folder,
                        error = %e,
                        "Failed to collect fetch results for search"
                    );
                    continue;
                }
            },
            Err(e) => {
                warn!(
                    account,
                    folder = %folder,
                    error = %e,
                    "Failed to fetch search result metadata"
                );
                continue;
            }
        };

        for fetch in &fetches {
            let uid = fetch.uid.unwrap_or(0);
            let flags: Vec<String> = fetch.flags().map(|f| format!("{f:?}")).collect();
            let unread = is_unread_flags(&flags);

            let (date, from_addr, subject) = if let Some(env) = fetch.envelope() {
                let date_str = env
                    .date
                    .as_ref()
                    .map(|d| cow_to_string(d))
                    .unwrap_or_default();
                let from_str = env
                    .from
                    .as_ref()
                    .and_then(|addrs| addrs.first())
                    .map(|addr| format_single_address(&addr.name, &addr.mailbox, &addr.host))
                    .unwrap_or_default();
                let subj_str = env
                    .subject
                    .as_ref()
                    .map(|s| cow_to_string(s))
                    .unwrap_or_else(|| "(no subject)".to_string());
                (date_str, from_str, subj_str)
            } else {
                (String::new(), String::new(), "(no envelope)".to_string())
            };

            let preview = fetch
                .text()
                .map(|t| {
                    let text_str = String::from_utf8_lossy(t);
                    text_preview(&text_str, 100)
                })
                .unwrap_or_default();

            all_results.push(serde_json::json!({
                "uid": uid,
                "account": account,
                "folder": folder,
                "sender": from_addr,
                "subject": subject,
                "date": date,
                "preview": preview,
                "unread": unread,
            }));
        }

        // Respect per-account limit
        if all_results.len() >= limit {
            all_results.truncate(limit);
            break;
        }
    }

    Ok(all_results)
}

pub struct EmailSearchTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
}

impl EmailSearchTool {
    pub fn new(connector: Arc<tokio::sync::Mutex<ImapConnector>>) -> Self {
        Self { connector }
    }
}

#[async_trait]
impl Tool for EmailSearchTool {
    fn name(&self) -> &str {
        "email.search"
    }

    fn description(&self) -> &str {
        "Search for emails across one or all accounts"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
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
                    "description": "Maximum results per account (integer, default: 20, max: 100)",
                    "minimum": 1,
                    "maximum": 100
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
            },
            "required": []
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let query = input["query"].as_str();
        let account_filter = input["account"].as_str();
        let folder_input = input["folder"].as_str().unwrap_or("INBOX");
        let limit = coerce_to_u64(&input["limit"]).unwrap_or(20).min(100) as usize;
        let since = input["since"].as_str();
        let from = input["from"].as_str();
        let unread_only = input["unread_only"].as_bool().unwrap_or(false);

        // Build IMAP search criteria
        let criteria = match build_search_criteria(query, since, from, unread_only) {
            Ok(c) => c,
            Err(msg) => {
                return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64));
            }
        };

        let mut connector = self.connector.lock().await;

        // Determine which accounts to search (caller-scoped, not the baked
        // loading-user snapshot — see `resolve_account_names`).
        let account_names: Vec<String> = if let Some(acct) = account_filter {
            vec![acct.to_string()]
        } else {
            connector.resolve_account_names()
        };

        // Determine which folders to search
        let search_all_folders = folder_input.eq_ignore_ascii_case("all");

        let mut all_messages = Vec::new();
        let mut errors = Vec::new();

        for acct_name in &account_names {
            // Resolve folders for this account
            let folders: Vec<String> = if search_all_folders {
                // List all folders for this account
                match list_account_folders(&mut connector, acct_name).await {
                    Ok(f) => f,
                    Err(e) => {
                        warn!(account = %acct_name, error = %e, "Failed to list folders for search");
                        errors.push(serde_json::json!({
                            "account": acct_name,
                            "error": e.to_string(),
                        }));
                        continue;
                    }
                }
            } else {
                vec![folder_input.to_string()]
            };

            match search_account(&mut connector, acct_name, &folders, &criteria, limit).await {
                Ok(msgs) => all_messages.extend(msgs),
                Err(e) => {
                    warn!(account = %acct_name, error = %e, "Failed to search account");
                    errors.push(serde_json::json!({
                        "account": acct_name,
                        "error": e.to_string(),
                    }));
                }
            }
        }

        // Sort by date (newest first) — use RFC2822-style date parsing
        // Dates from IMAP envelopes are typically RFC2822 format
        all_messages.sort_by(|a, b| {
            let date_a = a["date"].as_str().unwrap_or("");
            let date_b = b["date"].as_str().unwrap_or("");
            // Parse RFC2822 dates for comparison; fall back to string comparison
            let parsed_a = chrono::DateTime::parse_from_rfc2822(date_a).ok();
            let parsed_b = chrono::DateTime::parse_from_rfc2822(date_b).ok();
            match (parsed_a, parsed_b) {
                (Some(a), Some(b)) => b.cmp(&a), // newest first
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => date_b.cmp(date_a), // fallback string comparison
            }
        });

        // Global limit
        all_messages.truncate(limit);

        let output = serde_json::json!({
            "messages": all_messages,
            "count": all_messages.len(),
            "criteria": criteria,
            "errors": errors,
        });

        Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        ))
    }
}

/// List all folder names for an account via IMAP LIST.
async fn list_account_folders(
    connector: &mut ImapConnector,
    account: &str,
) -> std::result::Result<Vec<String>, super::error::EmailError> {
    let client = connector.get(Some(account)).await?;
    let session = client.session().await?;

    let mailboxes: Vec<_> = session
        .list(Some(""), Some("*"))
        .await
        .map_err(|e| super::error::EmailError::Imap(format!("Failed to list folders: {e}")))?
        .try_collect()
        .await
        .map_err(|e| {
            super::error::EmailError::Imap(format!("Failed to collect folder list: {e}"))
        })?;

    Ok(mailboxes.iter().map(|m| m.name().to_string()).collect())
}

// ── email.send ───────────────────────────────────────────────────────

use super::config::EmailAccountConfig;
use super::error::EmailError;
use lettre::message::{Mailbox, MessageBuilder};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

/// Parse a comma-separated list of email addresses and validate each one.
fn parse_and_validate_addresses(input: &str) -> Result<Vec<Mailbox>, EmailError> {
    let mut mailboxes = Vec::new();
    for raw in input.split(',') {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Try to parse as a full mailbox (with optional display name)
        match trimmed.parse::<Mailbox>() {
            Ok(mbox) => mailboxes.push(mbox),
            Err(_) => {
                return Err(EmailError::InvalidRecipient {
                    address: trimmed.to_string(),
                });
            }
        }
    }
    Ok(mailboxes)
}

/// Build a `From` mailbox from account config.
fn build_from_mailbox(config: &EmailAccountConfig) -> Result<Mailbox, EmailError> {
    let addr: lettre::Address =
        config
            .username
            .parse()
            .map_err(|_| EmailError::InvalidRecipient {
                address: config.username.clone(),
            })?;
    Ok(Mailbox::new(config.display_name.clone(), addr))
}

/// Determine the correct Sent folder name based on the IMAP host.
fn sent_folder_name(imap_host: &str) -> &str {
    provider_folder(imap_host, LogicalFolder::Sent)
}

/// Prepend "Re: " to a subject if not already present.
fn reply_subject(subject: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.to_lowercase().starts_with("re:") {
        trimmed.to_string()
    } else {
        format!("Re: {trimmed}")
    }
}

/// Build an SMTP transport for the given account configuration.
///
/// Supports three modes:
/// - `tls=true, port=465` → implicit TLS (SMTPS)
/// - `tls=true, port=587+` → STARTTLS
/// - `tls=false` → plaintext (for local services like ProtonMail Bridge)
pub(crate) async fn build_smtp_transport(
    config: &EmailAccountConfig,
) -> Result<AsyncSmtpTransport<Tokio1Executor>, EmailError> {
    let creds = Credentials::new(config.username.clone(), config.password.clone());
    let timeout = std::time::Duration::from_secs(config.command_timeout_secs as u64);

    let builder = if !config.tls {
        // Plaintext — for local services like ProtonMail Bridge on localhost
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.smtp_host)
            .port(config.smtp_port)
    } else if config.smtp_port == 465 {
        // Implicit TLS (port 465)
        AsyncSmtpTransport::<Tokio1Executor>::relay(&config.smtp_host)
            .map_err(|e| EmailError::SmtpTlsHandshake {
                host: config.smtp_host.clone(),
                detail: e.to_string(),
            })?
            .port(465)
    } else {
        // STARTTLS (port 587 or other)
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.smtp_host)
            .map_err(|e| EmailError::SmtpTlsHandshake {
                host: config.smtp_host.clone(),
                detail: e.to_string(),
            })?
            .port(config.smtp_port)
    };

    Ok(builder.credentials(creds).timeout(Some(timeout)).build())
}

/// Fetch the Message-ID, References, and Subject headers from an existing message
/// for building a reply.
async fn fetch_reply_headers(
    connector: &mut ImapConnector,
    account: &str,
    uid: u32,
    folder: &str,
) -> Result<(String, String, String)> {
    let fetch = fetch_message(
        connector,
        account,
        folder,
        uid,
        "BODY.PEEK[HEADER.FIELDS (MESSAGE-ID REFERENCES SUBJECT)]",
    )
    .await?;

    // Parse the raw header bytes
    let header_bytes = fetch.header().unwrap_or(b"");
    let headers = mailparse::parse_headers(header_bytes)
        .map(|(h, _)| h)
        .unwrap_or_default();

    let mut message_id = String::new();
    let mut references = String::new();
    let mut subject = String::new();

    for header in &headers {
        let key = header.get_key().to_lowercase();
        let val = header.get_value().trim().to_string();
        match key.as_str() {
            "message-id" => message_id = val,
            "references" => references = val,
            "subject" => subject = val,
            _ => {}
        }
    }

    Ok((message_id, references, subject))
}

/// Save a copy of the sent message to the Sent folder via IMAP APPEND.
/// Best-effort: logs a warning and returns Ok(()) on failure.
async fn save_to_sent_folder(
    connector: &mut ImapConnector,
    account: &str,
    raw_message: &[u8],
    imap_host: &str,
) -> Result<(), EmailError> {
    let folder = sent_folder_name(imap_host);
    debug!(account, folder, "Saving sent copy via IMAP APPEND");

    let client = match connector.get(Some(account)).await {
        Ok(c) => c,
        Err(e) => {
            warn!(account, error = %e, "Failed to get IMAP client for Sent folder save");
            return Ok(());
        }
    };
    let session = match client.session().await {
        Ok(s) => s,
        Err(e) => {
            warn!(account, error = %e, "Failed to get IMAP session for Sent folder save");
            return Ok(());
        }
    };

    match session
        .append(folder, Some("(\\Seen)"), None, raw_message)
        .await
    {
        Ok(_) => {
            debug!(account, folder, "Sent copy saved successfully");
        }
        Err(e) => {
            warn!(
                account,
                folder,
                error = %e,
                "Failed to save sent copy (non-fatal)"
            );
        }
    }

    Ok(())
}

pub struct EmailSendTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
    staged: StagedEmailStore,
    contacts: ContactsStore,
    rules: RulesStore,
}

impl EmailSendTool {
    pub fn new(
        connector: Arc<tokio::sync::Mutex<ImapConnector>>,
        staged: StagedEmailStore,
        contacts: ContactsStore,
        rules: RulesStore,
    ) -> Self {
        Self {
            connector,
            staged,
            contacts,
            rules,
        }
    }
}

/// Outbound attachment cap for SMTP accounts — the common provider limit.
const MAX_SMTP_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

fn staged_parts(input: &serde_json::Value) -> std::result::Result<Vec<MailPart>, String> {
    resolve_artifact_attachments(input, MAX_SMTP_ATTACHMENT_BYTES, "the 25 MB SMTP limit")
        .map(|staged| staged.into_iter().map(MailPart::from).collect())
}

#[async_trait]
impl Tool for EmailSendTool {
    fn name(&self) -> &str {
        "email.send"
    }

    fn surface_effect(
        &self,
        input: &serde_json::Value,
    ) -> Option<workforce_agent::tools::SurfaceEffect> {
        Some(workforce_agent::tools::SurfaceEffect::email(input))
    }

    fn description(&self) -> &str {
        "Send email with text/HTML, CC/BCC, reply threading"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
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
                    "description": "UID of message to reply to (integer, not string)",
                    "minimum": 1
                },
                "folder": {
                    "type": "string",
                    "description": "Folder containing the reply-to message (default: INBOX)"
                },
                "attach_artifact_ids": artifact_attachments_schema("html")
            },
            "required": ["to", "subject", "body"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        // ── Parse required parameters ────────────────────────────────
        let to_str = match input["to"].as_str() {
            Some(t) if !t.trim().is_empty() => t,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'to'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let subject_input = match input["subject"].as_str() {
            Some(s) => s.to_string(),
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'subject'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let body = match input["body"].as_str() {
            Some(b) => b,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'body'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // ── Parse optional parameters ────────────────────────────────
        let html = input["html"].as_str();
        let cc_str = input["cc"].as_str();
        let bcc_str = input["bcc"].as_str();
        let reply_to_uid = coerce_to_u64(&input["reply_to_uid"]).map(|u| u as u32);
        let folder = input["folder"].as_str().unwrap_or("INBOX");

        // ── Resolve account config ───────────────────────────────────
        let mut connector = self.connector.lock().await;
        let account_name = input["account"]
            .as_str()
            .unwrap_or(connector.default_account());
        let account_name = account_name.to_string();

        let config = match connector.resolve_account_config(&account_name) {
            Some(c) => c.clone(),
            None => {
                return Ok(ToolResult::failure(
                    format!(
                        "Email account '{}' not found — check configured account names",
                        account_name
                    ),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // ── Validate recipient addresses ─────────────────────────────
        let to_mailboxes = match parse_and_validate_addresses(to_str) {
            Ok(m) if m.is_empty() => {
                return Ok(ToolResult::failure(
                    "No valid recipient addresses in 'to'",
                    start.elapsed().as_millis() as u64,
                ));
            }
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let cc_mailboxes = if let Some(cc) = cc_str {
            match parse_and_validate_addresses(cc) {
                Ok(m) => m,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Invalid CC address: {e}"),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else {
            Vec::new()
        };

        let bcc_mailboxes = if let Some(bcc) = bcc_str {
            match parse_and_validate_addresses(bcc) {
                Ok(m) => m,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Invalid BCC address: {e}"),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else {
            Vec::new()
        };

        // ── Handle reply threading ───────────────────────────────────
        let mut subject = subject_input;
        let mut in_reply_to = None;
        let mut references_header = None;

        if let Some(uid) = reply_to_uid {
            match fetch_reply_headers(&mut connector, &account_name, uid, folder).await {
                Ok((orig_msg_id, orig_refs, orig_subject)) => {
                    // Set In-Reply-To
                    if !orig_msg_id.is_empty() {
                        in_reply_to = Some(orig_msg_id.clone());

                        // Build References header
                        let refs = if orig_refs.is_empty() {
                            orig_msg_id
                        } else {
                            format!("{orig_refs} {orig_msg_id}")
                        };
                        references_header = Some(refs);
                    }

                    // Use reply subject if user hasn't customised it
                    if !orig_subject.is_empty() && subject == orig_subject {
                        subject = reply_subject(&orig_subject);
                    } else if !subject.to_lowercase().starts_with("re:") {
                        subject = reply_subject(&subject);
                    }
                }
                Err(e) => {
                    warn!(uid, folder, error = %e, "Failed to fetch reply headers — sending without threading");
                }
            }
        }

        // ── Build the email message ──────────────────────────────────
        let from_mailbox = match build_from_mailbox(&config) {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid From address: {e}"),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let mut builder: MessageBuilder = lettre::Message::builder()
            .from(from_mailbox)
            .subject(&subject);

        // Add To recipients
        for mbox in &to_mailboxes {
            builder = builder.to(mbox.clone());
        }

        // Add CC recipients
        for mbox in &cc_mailboxes {
            builder = builder.cc(mbox.clone());
        }

        // Add BCC recipients
        for mbox in &bcc_mailboxes {
            builder = builder.bcc(mbox.clone());
        }

        // Add reply threading headers
        if let Some(ref irt) = in_reply_to {
            builder = builder.in_reply_to(irt.clone());
        }
        if let Some(ref refs) = references_header {
            builder = builder.references(refs.clone());
        }

        let html_body = match html {
            Some(h) => h.to_string(),
            None => markdown_to_html(body),
        };
        let parts = match staged_parts(&input) {
            Ok(parts) => parts,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let message = builder
            .multipart(assemble(body, Some(&html_body), &parts))
            .map_err(|e| eyre::eyre!("Failed to build email: {e}"))?;

        let raw_message = message.formatted();

        // ── Collect all recipient emails for security checks ────────
        let all_to: Vec<String> = to_mailboxes.iter().map(|m| m.email.to_string()).collect();
        let all_cc: Vec<String> = cc_mailboxes.iter().map(|m| m.email.to_string()).collect();
        let all_bcc: Vec<String> = bcc_mailboxes.iter().map(|m| m.email.to_string()).collect();
        let mut all_recipients = all_to.clone();
        all_recipients.extend(all_cc.clone());
        all_recipients.extend(all_bcc.clone());

        // ── Check pre-approval rules ────────────────────────────────
        let rules = self.rules.lock().await;
        let pre_approved = all_recipients
            .iter()
            .all(|addr| rules.matches(addr, &subject));
        drop(rules);

        if pre_approved {
            let result = send_staged_email(
                &message,
                &raw_message,
                &account_name,
                &config,
                &self.connector,
            )
            .await;
            return match result {
                Ok(message_id) => {
                    let recipient_count = all_recipients.len();
                    let output = serde_json::json!({
                        "success": true,
                        "message_id": message_id,
                        "account": account_name,
                        "to": to_str,
                        "subject": subject,
                        "recipients": recipient_count,
                        "cc": cc_str.unwrap_or(""),
                        "bcc": bcc_str.unwrap_or(""),
                        "reply_to_uid": reply_to_uid,
                        "has_html": html.is_some(),
                        "pre_approved": true,
                    });
                    Ok(ToolResult::success(
                        output,
                        start.elapsed().as_millis() as u64,
                    ))
                }
                Err(e) => Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                )),
            };
        }

        // ── Stage the email for review ──────────────────────────────
        let contacts = self.contacts.lock().await;
        let unknown = check_unknown_recipients(&all_recipients, &contacts);
        drop(contacts);

        let mut flags = scan_sensitive_content(&subject, body);
        if !unknown.is_empty() {
            flags.push(format!("unknown_recipients: {}", unknown.join(", ")));
        }

        let body_preview = text_preview(body, 200);
        let preview = EmailPreview {
            to: all_to,
            cc: all_cc,
            bcc: all_bcc,
            subject: subject.clone(),
            body_preview,
            unknown_recipients: unknown,
        };

        let confirmation_id = uuid::Uuid::new_v4().to_string();
        let staged = StagedEmail {
            message,
            raw_message,
            account_name: account_name.clone(),
            config,
            preview: preview.clone(),
            staged_at: chrono::Utc::now(),
            flags: flags.clone(),
        };

        self.staged
            .lock()
            .await
            .insert(confirmation_id.clone(), staged);

        let output = serde_json::json!({
            "staged": true,
            "confirmation_id": confirmation_id,
            "account": account_name,
            "preview": preview,
            "flags": flags,
            "action_required": "Call email.confirm with this confirmation_id to send, or action='cancel' to discard.",
        });

        Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        ))
    }
}

async fn send_staged_email(
    message: &lettre::Message,
    raw_message: &[u8],
    account_name: &str,
    config: &super::config::EmailAccountConfig,
    connector: &Arc<tokio::sync::Mutex<ImapConnector>>,
) -> std::result::Result<String, String> {
    let message_id = message
        .headers()
        .get_raw("Message-ID")
        .map(|v| v.to_string())
        .unwrap_or_default();

    debug!(account = %account_name, "Building SMTP transport");
    let transport = build_smtp_transport(config)
        .await
        .map_err(|e| e.to_string())?;

    transport.send(message.clone()).await.map_err(|e| {
        let detail = e.to_string();
        if detail.to_lowercase().contains("authentication")
            || detail.to_lowercase().contains("credentials")
        {
            EmailError::SmtpAuthFailed {
                account: account_name.to_string(),
            }
            .to_string()
        } else {
            EmailError::SmtpSendFailed { detail }.to_string()
        }
    })?;

    debug!(account = %account_name, "Email sent successfully via SMTP");

    let mut conn = connector.lock().await;
    let _ = save_to_sent_folder(&mut conn, account_name, raw_message, &config.imap_host).await;

    Ok(message_id)
}

// ── email.draft ─────────────────────────────────────────────────────

pub struct EmailDraftTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
}

impl EmailDraftTool {
    pub fn new(connector: Arc<tokio::sync::Mutex<ImapConnector>>) -> Self {
        Self { connector }
    }
}

#[async_trait]
impl Tool for EmailDraftTool {
    fn name(&self) -> &str {
        "email.draft"
    }

    fn description(&self) -> &str {
        "Create a draft email and save it to the Drafts folder"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "account": {
                    "type": "string",
                    "description": "Account name to draft from. Uses default account if omitted."
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
                "attach_artifact_ids": artifact_attachments_schema("html")
            },
            "required": ["to", "subject", "body"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let to_str = match input["to"].as_str() {
            Some(t) if !t.trim().is_empty() => t,
            _ => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'to'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let subject = match input["subject"].as_str() {
            Some(s) => s.to_string(),
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'subject'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let body = match input["body"].as_str() {
            Some(b) => b,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'body'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let html = input["html"].as_str();
        let cc_str = input["cc"].as_str();
        let bcc_str = input["bcc"].as_str();

        let mut connector = self.connector.lock().await;
        let account_name = input["account"]
            .as_str()
            .unwrap_or(connector.default_account())
            .to_string();

        let config = match connector.resolve_account_config(&account_name) {
            Some(c) => c.clone(),
            None => {
                return Ok(ToolResult::failure(
                    format!("Email account '{}' not found", account_name),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let to_mailboxes = match parse_and_validate_addresses(to_str) {
            Ok(m) if m.is_empty() => {
                return Ok(ToolResult::failure(
                    "No valid recipient addresses in 'to'",
                    start.elapsed().as_millis() as u64,
                ));
            }
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let cc_mailboxes = cc_str
            .map(parse_and_validate_addresses)
            .transpose()
            .map_err(|e| eyre::eyre!("{e}"))?
            .unwrap_or_default();

        let bcc_mailboxes = bcc_str
            .map(parse_and_validate_addresses)
            .transpose()
            .map_err(|e| eyre::eyre!("{e}"))?
            .unwrap_or_default();

        let from_mailbox = match build_from_mailbox(&config) {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid From address: {e}"),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let mut builder = lettre::Message::builder()
            .from(from_mailbox)
            .subject(&subject);

        for mbox in &to_mailboxes {
            builder = builder.to(mbox.clone());
        }
        for mbox in &cc_mailboxes {
            builder = builder.cc(mbox.clone());
        }
        for mbox in &bcc_mailboxes {
            builder = builder.bcc(mbox.clone());
        }

        let html_body = match html {
            Some(h) => h.to_string(),
            None => markdown_to_html(body),
        };
        let parts = match staged_parts(&input) {
            Ok(parts) => parts,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        let message = builder
            .multipart(assemble(body, Some(&html_body), &parts))
            .map_err(|e| eyre::eyre!("Failed to build draft: {e}"))?;

        let raw_message = message.formatted();

        // Save to Drafts folder via IMAP APPEND
        let drafts_folder = provider_folder(&config.imap_host, LogicalFolder::Drafts);

        let client = connector
            .get(Some(&account_name))
            .await
            .map_err(|e| eyre::eyre!("Failed to get IMAP client: {e}"))?;
        let session = client
            .session()
            .await
            .map_err(|e| eyre::eyre!("Failed to get IMAP session: {e}"))?;

        match session
            .append(drafts_folder, Some("(\\Draft)"), None, &raw_message)
            .await
        {
            Ok(_) => {
                info!(account = %account_name, folder = %drafts_folder, "Draft saved");
                Ok(ToolResult::success(
                    serde_json::json!({
                        "success": true,
                        "account": account_name,
                        "folder": drafts_folder,
                        "to": to_str,
                        "subject": subject,
                        "message": "Draft saved — open your email client to review and send"
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to save draft: {e}"),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

// ── Provider folder mapping ─────────────────────────────────────────

/// Logical folder types for provider-specific mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogicalFolder {
    Archive,
    Trash,
    Sent,
    Drafts,
}

/// Map a logical folder to the provider-specific IMAP folder name.
fn provider_folder(imap_host: &str, folder: LogicalFolder) -> &'static str {
    let provider = infer_provider(imap_host);
    match (provider, folder) {
        ("Gmail", LogicalFolder::Archive) => "[Gmail]/All Mail",
        ("Gmail", LogicalFolder::Trash) => "[Gmail]/Trash",
        ("Gmail", LogicalFolder::Sent) => "[Gmail]/Sent Mail",
        ("Gmail", LogicalFolder::Drafts) => "[Gmail]/Drafts",
        ("iCloud", LogicalFolder::Archive) => "Archive",
        ("iCloud", LogicalFolder::Trash) => "Deleted Messages",
        ("iCloud", LogicalFolder::Sent) => "Sent Messages",
        ("iCloud", LogicalFolder::Drafts) => "Drafts",
        (_, LogicalFolder::Archive) => "Archive",
        (_, LogicalFolder::Trash) => "Trash",
        (_, LogicalFolder::Sent) => "Sent",
        (_, LogicalFolder::Drafts) => "Drafts",
    }
}

// ── Bulk UID helper ─────────────────────────────────────────────────

/// Parse `uid` (single) or `uids` (array) from tool input into a
/// comma-separated UID string for IMAP commands.
fn parse_uids(input: &serde_json::Value) -> Result<String, String> {
    // Try single uid first
    if let Some(uid) = coerce_to_u64(&input["uid"]) {
        if uid == 0 {
            return Err("UID must be a positive integer".to_string());
        }
        return Ok(uid.to_string());
    }

    // Try uids array
    if let Some(arr) = input["uids"].as_array() {
        if arr.is_empty() {
            return Err("'uids' array must not be empty".to_string());
        }
        let mut uids = Vec::with_capacity(arr.len());
        for val in arr {
            match coerce_to_u64(val) {
                Some(u) if u > 0 => uids.push(u.to_string()),
                Some(_) => return Err("UIDs must be positive integers".to_string()),
                None => {
                    return Err(format!(
                        "Invalid UID value: {val} — expected a positive integer"
                    ))
                }
            }
        }
        return Ok(uids.join(","));
    }

    Err(
        "Missing required parameter: provide 'uid' (integer) or 'uids' (array of integers)"
            .to_string(),
    )
}

// ── email.mark ──────────────────────────────────────────────────────

pub struct EmailMarkTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
}

impl EmailMarkTool {
    pub fn new(connector: Arc<tokio::sync::Mutex<ImapConnector>>) -> Self {
        Self { connector }
    }
}

/// Parse a mark action string into the IMAP flag query string.
fn parse_mark_action(action: &str) -> Result<&'static str, String> {
    match action {
        "read" => Ok("+FLAGS (\\Seen)"),
        "unread" => Ok("-FLAGS (\\Seen)"),
        "flag" => Ok("+FLAGS (\\Flagged)"),
        "unflag" => Ok("-FLAGS (\\Flagged)"),
        other => Err(format!(
            "Invalid action '{other}'. Valid actions: read, unread, flag, unflag"
        )),
    }
}

#[async_trait]
impl Tool for EmailMarkTool {
    fn name(&self) -> &str {
        "email.mark"
    }

    fn description(&self) -> &str {
        "Mark email as read or unread"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
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
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let account = match input["account"].as_str() {
            Some(a) => a,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'account'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let action = match input["action"].as_str() {
            Some(a) => a,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'action'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let flag_str = match parse_mark_action(action) {
            Ok(f) => f,
            Err(msg) => {
                return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64));
            }
        };

        let uid_range = match parse_uids(&input) {
            Ok(u) => u,
            Err(msg) => {
                return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64));
            }
        };

        let folder = input["folder"].as_str().unwrap_or("INBOX");

        let mut connector = self.connector.lock().await;
        let client = match connector.get(Some(account)).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let session = match client.session().await {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // SELECT for read-write access (not EXAMINE)
        session
            .select(folder)
            .await
            .map_err(|e| eyre::eyre!("Failed to select folder {folder}: {e}"))?;

        let fetches: Vec<_> = session
            .uid_store(&uid_range, flag_str)
            .await
            .map_err(|e| eyre::eyre!("Failed to store flags: {e}"))?
            .try_collect()
            .await
            .map_err(|e| eyre::eyre!("Failed to collect store results: {e}"))?;

        let affected = fetches.len();

        let output = serde_json::json!({
            "success": true,
            "action": action,
            "affected": affected,
            "folder": folder,
            "account": account,
        });

        Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ── email.move ──────────────────────────────────────────────────────

pub struct EmailMoveTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
}

impl EmailMoveTool {
    pub fn new(connector: Arc<tokio::sync::Mutex<ImapConnector>>) -> Self {
        Self { connector }
    }
}

#[async_trait]
impl Tool for EmailMoveTool {
    fn name(&self) -> &str {
        "email.move"
    }

    fn description(&self) -> &str {
        "Move email between folders"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
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
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let account = match input["account"].as_str() {
            Some(a) => a,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'account'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let to_folder = match input["to_folder"].as_str() {
            Some(f) => f,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'to_folder'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let uid_range = match parse_uids(&input) {
            Ok(u) => u,
            Err(msg) => {
                return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64));
            }
        };

        let from_folder = input["from_folder"].as_str().unwrap_or("INBOX");

        let mut connector = self.connector.lock().await;

        // Validate destination folder exists
        let folders = list_account_folders(&mut connector, account)
            .await
            .map_err(|e| eyre::eyre!("{e}"))?;

        if !folders.iter().any(|f| f == to_folder) {
            let err = super::error::EmailError::FolderNotFound {
                folder: to_folder.to_string(),
                available: folders,
            };
            return Ok(ToolResult::failure(
                err.to_string(),
                start.elapsed().as_millis() as u64,
            ));
        }

        let client = match connector.get(Some(account)).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let session = match client.session().await {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // SELECT source folder for read-write access
        session
            .select(from_folder)
            .await
            .map_err(|e| eyre::eyre!("Failed to select folder {from_folder}: {e}"))?;

        // COPY to destination
        session.uid_copy(&uid_range, to_folder).await.map_err(|e| {
            eyre::eyre!(
                "{}",
                super::error::EmailError::MoveFailure {
                    detail: format!("COPY to '{to_folder}' failed: {e}"),
                }
            )
        })?;

        // Mark originals as deleted
        let _: Vec<_> = session
            .uid_store(&uid_range, "+FLAGS (\\Deleted)")
            .await
            .map_err(|e| eyre::eyre!("Failed to mark messages as deleted: {e}"))?
            .try_collect()
            .await
            .map_err(|e| eyre::eyre!("Failed to collect store results: {e}"))?;

        // Expunge to permanently remove from source
        let _: Vec<u32> = session
            .expunge()
            .await
            .map_err(|e| eyre::eyre!("Failed to expunge messages: {e}"))?
            .try_collect()
            .await
            .map_err(|e| eyre::eyre!("Failed to collect expunge results: {e}"))?;

        let output = serde_json::json!({
            "success": true,
            "from_folder": from_folder,
            "to_folder": to_folder,
            "account": account,
        });

        Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ── email.archive ───────────────────────────────────────────────────

pub struct EmailArchiveTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
}

impl EmailArchiveTool {
    pub fn new(connector: Arc<tokio::sync::Mutex<ImapConnector>>) -> Self {
        Self { connector }
    }
}

#[async_trait]
impl Tool for EmailArchiveTool {
    fn name(&self) -> &str {
        "email.archive"
    }

    fn description(&self) -> &str {
        "Archive email messages"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
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
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let account = match input["account"].as_str() {
            Some(a) => a,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'account'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let uid_range = match parse_uids(&input) {
            Ok(u) => u,
            Err(msg) => {
                return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64));
            }
        };

        let folder = input["folder"].as_str().unwrap_or("INBOX");

        let mut connector = self.connector.lock().await;

        // Resolve archive folder from provider
        let imap_host = match connector.resolve_account_config(account) {
            Some(cfg) => cfg.imap_host.clone(),
            None => {
                return Ok(ToolResult::failure(
                    format!("Email account '{account}' not found — check configured account names"),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };
        let archive_folder = provider_folder(&imap_host, LogicalFolder::Archive);

        // Validate archive folder exists
        let folders = list_account_folders(&mut connector, account)
            .await
            .map_err(|e| eyre::eyre!("{e}"))?;

        if !folders.iter().any(|f| f == archive_folder) {
            let err = super::error::EmailError::FolderNotFound {
                folder: archive_folder.to_string(),
                available: folders,
            };
            return Ok(ToolResult::failure(
                err.to_string(),
                start.elapsed().as_millis() as u64,
            ));
        }

        let client = match connector.get(Some(account)).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let session = match client.session().await {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // SELECT source folder for read-write access
        session
            .select(folder)
            .await
            .map_err(|e| eyre::eyre!("Failed to select folder {folder}: {e}"))?;

        // COPY to archive folder
        session
            .uid_copy(&uid_range, archive_folder)
            .await
            .map_err(|e| {
                eyre::eyre!(
                    "{}",
                    super::error::EmailError::MoveFailure {
                        detail: format!("COPY to '{archive_folder}' failed: {e}"),
                    }
                )
            })?;

        // Mark originals as deleted
        let _: Vec<_> = session
            .uid_store(&uid_range, "+FLAGS (\\Deleted)")
            .await
            .map_err(|e| eyre::eyre!("Failed to mark messages as deleted: {e}"))?
            .try_collect()
            .await
            .map_err(|e| eyre::eyre!("Failed to collect store results: {e}"))?;

        // Expunge
        let _: Vec<u32> = session
            .expunge()
            .await
            .map_err(|e| eyre::eyre!("Failed to expunge messages: {e}"))?
            .try_collect()
            .await
            .map_err(|e| eyre::eyre!("Failed to collect expunge results: {e}"))?;

        let output = serde_json::json!({
            "success": true,
            "from_folder": folder,
            "archive_folder": archive_folder,
            "account": account,
        });

        Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ── email.delete ────────────────────────────────────────────────────

pub struct EmailDeleteTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
}

impl EmailDeleteTool {
    pub fn new(connector: Arc<tokio::sync::Mutex<ImapConnector>>) -> Self {
        Self { connector }
    }
}

#[async_trait]
impl Tool for EmailDeleteTool {
    fn name(&self) -> &str {
        "email.delete"
    }

    fn description(&self) -> &str {
        "Delete email messages"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
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
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let account = match input["account"].as_str() {
            Some(a) => a,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'account'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let uid_range = match parse_uids(&input) {
            Ok(u) => u,
            Err(msg) => {
                return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64));
            }
        };

        let folder = input["folder"].as_str().unwrap_or("INBOX");
        let permanent = input["permanent"].as_bool().unwrap_or(false);

        let mut connector = self.connector.lock().await;

        if permanent {
            // Permanent delete: STORE \Deleted + EXPUNGE directly
            let client = match connector.get(Some(account)).await {
                Ok(c) => c,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        e.to_string(),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            };

            let session = match client.session().await {
                Ok(s) => s,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        e.to_string(),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            };

            session
                .select(folder)
                .await
                .map_err(|e| eyre::eyre!("Failed to select folder {folder}: {e}"))?;

            let _: Vec<_> = session
                .uid_store(&uid_range, "+FLAGS (\\Deleted)")
                .await
                .map_err(|e| eyre::eyre!("Failed to mark messages as deleted: {e}"))?
                .try_collect()
                .await
                .map_err(|e| eyre::eyre!("Failed to collect store results: {e}"))?;

            let _: Vec<u32> = session
                .expunge()
                .await
                .map_err(|e| eyre::eyre!("Failed to expunge messages: {e}"))?
                .try_collect()
                .await
                .map_err(|e| eyre::eyre!("Failed to collect expunge results: {e}"))?;

            let output = serde_json::json!({
                "success": true,
                "permanent": true,
                "folder": folder,
                "account": account,
            });

            Ok(ToolResult::success(
                output,
                start.elapsed().as_millis() as u64,
            ))
        } else {
            // Move to Trash: COPY → STORE \Deleted → EXPUNGE
            let imap_host = match connector.resolve_account_config(account) {
                Some(cfg) => cfg.imap_host.clone(),
                None => {
                    return Ok(ToolResult::failure(
                        format!(
                            "Email account '{account}' not found — check configured account names"
                        ),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            };
            let trash_folder = provider_folder(&imap_host, LogicalFolder::Trash);

            // Validate trash folder exists
            let folders = list_account_folders(&mut connector, account)
                .await
                .map_err(|e| eyre::eyre!("{e}"))?;

            if !folders.iter().any(|f| f == trash_folder) {
                let err = super::error::EmailError::FolderNotFound {
                    folder: trash_folder.to_string(),
                    available: folders,
                };
                return Ok(ToolResult::failure(
                    err.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }

            let client = match connector.get(Some(account)).await {
                Ok(c) => c,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        e.to_string(),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            };

            let session = match client.session().await {
                Ok(s) => s,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        e.to_string(),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            };

            session
                .select(folder)
                .await
                .map_err(|e| eyre::eyre!("Failed to select folder {folder}: {e}"))?;

            session
                .uid_copy(&uid_range, trash_folder)
                .await
                .map_err(|e| {
                    eyre::eyre!(
                        "{}",
                        super::error::EmailError::MoveFailure {
                            detail: format!("COPY to '{trash_folder}' failed: {e}"),
                        }
                    )
                })?;

            let _: Vec<_> = session
                .uid_store(&uid_range, "+FLAGS (\\Deleted)")
                .await
                .map_err(|e| eyre::eyre!("Failed to mark messages as deleted: {e}"))?
                .try_collect()
                .await
                .map_err(|e| eyre::eyre!("Failed to collect store results: {e}"))?;

            let _: Vec<u32> = session
                .expunge()
                .await
                .map_err(|e| eyre::eyre!("Failed to expunge messages: {e}"))?
                .try_collect()
                .await
                .map_err(|e| eyre::eyre!("Failed to collect expunge results: {e}"))?;

            let output = serde_json::json!({
                "success": true,
                "permanent": false,
                "folder": folder,
                "trash_folder": trash_folder,
                "account": account,
            });

            Ok(ToolResult::success(
                output,
                start.elapsed().as_millis() as u64,
            ))
        }
    }
}

// ── email.forward ────────────────────────────────────────────────────

/// Prefix "Fwd: " to a subject if not already present (case-insensitive).
fn forward_subject(subject: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.to_lowercase().starts_with("fwd:") {
        trimmed.to_string()
    } else {
        format!("Fwd: {trimmed}")
    }
}

/// Maximum allowed total attachment size when forwarding (25 MB).
const MAX_FORWARD_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

/// The parts a forward re-attaches: attachment-disposition parts only, never
/// inline ones, and none when the caller opted out.
fn forwardable_attachments(parts: Vec<MimePart>, include: bool) -> Vec<MimePart> {
    parts.into_iter().filter(|p| include && !p.inline).collect()
}

pub struct EmailForwardTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
    staged: StagedEmailStore,
    contacts: ContactsStore,
    rules: RulesStore,
}

impl EmailForwardTool {
    pub fn new(
        connector: Arc<tokio::sync::Mutex<ImapConnector>>,
        staged: StagedEmailStore,
        contacts: ContactsStore,
        rules: RulesStore,
    ) -> Self {
        Self {
            connector,
            staged,
            contacts,
            rules,
        }
    }
}

#[async_trait]
impl Tool for EmailForwardTool {
    fn name(&self) -> &str {
        "email.forward"
    }

    fn surface_effect(
        &self,
        input: &serde_json::Value,
    ) -> Option<workforce_agent::tools::SurfaceEffect> {
        Some(workforce_agent::tools::SurfaceEffect::email(input))
    }

    fn description(&self) -> &str {
        "Forward an email to new recipients, preserving original metadata and attachments"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
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
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        // ── Parse required parameters ────────────────────────────────
        let MessageLocator {
            account,
            uid,
            folder,
        } = match message_locator(&input) {
            Ok(l) => l,
            Err(msg) => return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64)),
        };

        let to_str = match input["to"].as_str() {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'to'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // ── Parse optional parameters ────────────────────────────────
        let cc_str = input["cc"].as_str().map(|s| s.to_string());
        let bcc_str = input["bcc"].as_str().map(|s| s.to_string());
        let comment = input["comment"].as_str().map(|s| s.to_string());
        let include_attachments = input["include_attachments"].as_bool().unwrap_or(true);

        // ── Resolve account configs ──────────────────────────────────
        let mut connector = self.connector.lock().await;

        let send_from = input["send_from"].as_str().unwrap_or(account).to_string();

        let source_config = match connector.resolve_account_config(account) {
            Some(c) => c.clone(),
            None => {
                return Ok(ToolResult::failure(
                    format!("Email account '{account}' not found — check configured account names"),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let send_config = if send_from == account {
            source_config.clone()
        } else {
            match connector.resolve_account_config(&send_from) {
                Some(c) => c.clone(),
                None => {
                    return Ok(ToolResult::failure(
                        format!("Send-from account '{send_from}' not found"),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        };

        // ── Fetch the original message via IMAP ──────────────────────
        let fetch = match fetch_message(
            &mut connector,
            account,
            folder,
            uid,
            "(FLAGS ENVELOPE BODY[])",
        )
        .await
        {
            Ok(f) => f,
            Err(e) => {
                return Ok(ToolResult::from_report(
                    &e,
                    start.elapsed().as_millis() as u64,
                ))
            }
        };

        // ── Extract envelope metadata ────────────────────────────────
        let (orig_from, orig_to, orig_date, orig_subject) = if let Some(env) = fetch.envelope() {
            let from_str = format_address_list(env.from.as_deref());
            let to_env = format_address_list(env.to.as_deref());
            let date_str = env
                .date
                .as_ref()
                .map(|d| cow_to_string(d))
                .unwrap_or_default();
            let subj_str = env
                .subject
                .as_ref()
                .map(|s| cow_to_string(s))
                .unwrap_or_else(|| "(no subject)".to_string());
            (from_str, to_env, date_str, subj_str)
        } else {
            (
                String::new(),
                String::new(),
                String::new(),
                "(no subject)".to_string(),
            )
        };

        // ── Parse the MIME body ──────────────────────────────────────
        let raw_body = fetch.body().unwrap_or(b"");
        let MimeContent {
            text: orig_text,
            html: orig_html,
            parts,
        } = mailparse::parse_mail(raw_body)
            .map(|mail| walk_mime(&mail))
            .unwrap_or_else(|e| {
                warn!(error = %e, uid, "Failed to parse MIME body for forward — using raw bytes");
                MimeContent {
                    text: String::from_utf8_lossy(raw_body).to_string(),
                    ..MimeContent::default()
                }
            });
        let fwd_attachments = forwardable_attachments(parts, include_attachments);

        // ── Check total attachment size ──────────────────────────────
        let total_att_bytes: usize = fwd_attachments.iter().map(|a| a.data.len()).sum();
        if total_att_bytes > MAX_FORWARD_ATTACHMENT_BYTES {
            return Ok(ToolResult::failure(
                format!(
                    "Total attachment size ({}) exceeds the {} MB limit — set include_attachments=false to forward without attachments",
                    format_size(total_att_bytes),
                    MAX_FORWARD_ATTACHMENT_BYTES / (1024 * 1024),
                ),
                start.elapsed().as_millis() as u64,
            ));
        }

        // ── Validate recipient addresses ─────────────────────────────
        let to_mailboxes = match parse_and_validate_addresses(&to_str) {
            Ok(m) if m.is_empty() => {
                return Ok(ToolResult::failure(
                    "No valid recipient addresses in 'to'",
                    start.elapsed().as_millis() as u64,
                ));
            }
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let cc_mailboxes = if let Some(ref cc) = cc_str {
            match parse_and_validate_addresses(cc) {
                Ok(m) => m,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Invalid CC address: {e}"),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else {
            Vec::new()
        };

        let bcc_mailboxes = if let Some(ref bcc) = bcc_str {
            match parse_and_validate_addresses(bcc) {
                Ok(m) => m,
                Err(e) => {
                    return Ok(ToolResult::failure(
                        format!("Invalid BCC address: {e}"),
                        start.elapsed().as_millis() as u64,
                    ));
                }
            }
        } else {
            Vec::new()
        };

        // ── Build forwarded subject ──────────────────────────────────
        let subject = forward_subject(&orig_subject);

        // ── Build forwarded header block (text version) ──────────────
        let fwd_header_text = format!(
            "---------- Forwarded message ----------\nFrom: {orig_from}\nDate: {orig_date}\nSubject: {orig_subject}\nTo: {orig_to}\n"
        );

        // ── Build forwarded header block (HTML version) ──────────────
        let fwd_header_html = format!(
            "<div style=\"border-left:2px solid #ccc;padding-left:8px;color:#555\">\
             <b>---------- Forwarded message ----------</b><br>\
             <b>From:</b> {orig_from}<br>\
             <b>Date:</b> {orig_date}<br>\
             <b>Subject:</b> {orig_subject}<br>\
             <b>To:</b> {orig_to}<br>\
             </div><br>"
        );

        // ── Assemble plain-text body ─────────────────────────────────
        let plain_body = {
            let mut buf = String::new();
            if let Some(ref c) = comment {
                buf.push_str(c.trim());
                buf.push_str("\n\n");
            }
            buf.push_str(&fwd_header_text);
            buf.push('\n');
            buf.push_str(&orig_text);
            buf
        };

        // ── Assemble HTML body (only if original had HTML) ───────────
        let html_body_opt: Option<String> = if !orig_html.is_empty() {
            let mut buf = String::new();
            if let Some(ref c) = comment {
                buf.push_str("<p>");
                buf.push_str(c.trim());
                buf.push_str("</p>\n");
            }
            buf.push_str(&fwd_header_html);
            buf.push_str(&orig_html);
            Some(buf)
        } else {
            None
        };

        // ── Build the From mailbox for the sender ────────────────────
        let from_mailbox = match build_from_mailbox(&send_config) {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid From address: {e}"),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let mut builder: MessageBuilder = lettre::Message::builder()
            .from(from_mailbox)
            .subject(&subject);

        for mbox in &to_mailboxes {
            builder = builder.to(mbox.clone());
        }
        for mbox in &cc_mailboxes {
            builder = builder.cc(mbox.clone());
        }
        for mbox in &bcc_mailboxes {
            builder = builder.bcc(mbox.clone());
        }

        let parts: Vec<MailPart> = fwd_attachments
            .into_iter()
            .map(|att| MailPart::attached(att.filename, att.mime_type, att.data))
            .collect();
        let message = builder
            .multipart(assemble(&plain_body, html_body_opt.as_deref(), &parts))
            .map_err(|e| eyre::eyre!("Failed to build forward message: {e}"))?;

        let raw_message = message.formatted();

        // ── Collect all recipient emails for security checks ────────
        let all_to: Vec<String> = to_mailboxes.iter().map(|m| m.email.to_string()).collect();
        let all_cc: Vec<String> = cc_mailboxes.iter().map(|m| m.email.to_string()).collect();
        let all_bcc: Vec<String> = bcc_mailboxes.iter().map(|m| m.email.to_string()).collect();
        let mut all_recipients = all_to.clone();
        all_recipients.extend(all_cc.clone());
        all_recipients.extend(all_bcc.clone());

        let attachment_count = parts.len();

        // ── Check pre-approval rules ────────────────────────────────
        let rules = self.rules.lock().await;
        let pre_approved = all_recipients
            .iter()
            .all(|addr| rules.matches(addr, &subject));
        drop(rules);

        if pre_approved {
            let result = send_staged_email(
                &message,
                &raw_message,
                &send_from,
                &send_config,
                &self.connector,
            )
            .await;
            return match result {
                Ok(message_id) => {
                    let recipient_count = all_recipients.len();
                    let output = serde_json::json!({
                        "success": true,
                        "message_id": message_id,
                        "account": send_from,
                        "forwarded_uid": uid,
                        "forwarded_from_account": account,
                        "to": to_str,
                        "subject": subject,
                        "recipients": recipient_count,
                        "cc": cc_str.as_deref().unwrap_or(""),
                        "bcc": bcc_str.as_deref().unwrap_or(""),
                        "attachments_forwarded": attachment_count,
                        "has_html": html_body_opt.is_some(),
                        "pre_approved": true,
                    });
                    info!(
                        account = %send_from,
                        uid,
                        subject = %subject,
                        recipients = recipient_count,
                        attachments = attachment_count,
                        "Email forwarded successfully (pre-approved)"
                    );
                    Ok(ToolResult::success(
                        output,
                        start.elapsed().as_millis() as u64,
                    ))
                }
                Err(e) => Ok(ToolResult::failure(
                    e.to_string(),
                    start.elapsed().as_millis() as u64,
                )),
            };
        }

        // ── Stage the email for review ──────────────────────────────
        let contacts = self.contacts.lock().await;
        let unknown = check_unknown_recipients(&all_recipients, &contacts);
        drop(contacts);

        let mut flags = scan_sensitive_content(&subject, &plain_body);
        if !unknown.is_empty() {
            flags.push(format!("unknown_recipients: {}", unknown.join(", ")));
        }

        let body_preview = text_preview(&plain_body, 200);
        let preview = EmailPreview {
            to: all_to,
            cc: all_cc,
            bcc: all_bcc,
            subject: subject.clone(),
            body_preview,
            unknown_recipients: unknown,
        };

        let confirmation_id = uuid::Uuid::new_v4().to_string();
        let staged = StagedEmail {
            message,
            raw_message,
            account_name: send_from.clone(),
            config: send_config,
            preview: preview.clone(),
            staged_at: chrono::Utc::now(),
            flags: flags.clone(),
        };

        self.staged
            .lock()
            .await
            .insert(confirmation_id.clone(), staged);

        let output = serde_json::json!({
            "staged": true,
            "confirmation_id": confirmation_id,
            "account": send_from,
            "forwarded_uid": uid,
            "forwarded_from_account": account,
            "preview": preview,
            "flags": flags,
            "attachments_forwarded": attachment_count,
            "action_required": "Call email.confirm with this confirmation_id to send, or action='cancel' to discard.",
        });

        Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        ))
    }
}

// ── email.confirm ────────────────────────────────────────────────────

pub struct EmailConfirmTool {
    connector: Arc<tokio::sync::Mutex<ImapConnector>>,
    staged: StagedEmailStore,
}

impl EmailConfirmTool {
    pub fn new(
        connector: Arc<tokio::sync::Mutex<ImapConnector>>,
        staged: StagedEmailStore,
    ) -> Self {
        Self { connector, staged }
    }
}

#[async_trait]
impl Tool for EmailConfirmTool {
    fn name(&self) -> &str {
        "email.confirm"
    }

    fn description(&self) -> &str {
        "Confirm or cancel a staged email"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
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
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let confirmation_id = match input["confirmation_id"].as_str() {
            Some(id) if !id.trim().is_empty() => id.to_string(),
            _ => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'confirmation_id'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let action = input["action"].as_str().unwrap_or("send");

        let staged_email = self.staged.lock().await.remove(&confirmation_id);
        let staged_email = match staged_email {
            Some(s) => s,
            None => {
                return Ok(ToolResult::failure(
                    format!("No staged email found for confirmation_id '{confirmation_id}' — it may have expired or already been processed"),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        if action == "cancel" {
            let output = serde_json::json!({
                "cancelled": true,
                "confirmation_id": confirmation_id,
                "subject": staged_email.preview.subject,
            });
            return Ok(ToolResult::success(
                output,
                start.elapsed().as_millis() as u64,
            ));
        }

        let result = send_staged_email(
            &staged_email.message,
            &staged_email.raw_message,
            &staged_email.account_name,
            &staged_email.config,
            &self.connector,
        )
        .await;

        match result {
            Ok(message_id) => {
                let recipient_count = staged_email.preview.to.len()
                    + staged_email.preview.cc.len()
                    + staged_email.preview.bcc.len();
                let output = serde_json::json!({
                    "success": true,
                    "message_id": message_id,
                    "account": staged_email.account_name,
                    "subject": staged_email.preview.subject,
                    "recipients": recipient_count,
                });
                info!(
                    account = %staged_email.account_name,
                    subject = %staged_email.preview.subject,
                    recipients = recipient_count,
                    "Staged email sent successfully"
                );
                Ok(ToolResult::success(
                    output,
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                e.to_string(),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

// ── email.contacts ──────────────────────────────────────────────────

pub struct EmailContactsTool {
    contacts: ContactsStore,
}

impl EmailContactsTool {
    pub fn new(contacts: ContactsStore) -> Self {
        Self { contacts }
    }
}

#[async_trait]
impl Tool for EmailContactsTool {
    fn name(&self) -> &str {
        "email.contacts"
    }

    fn description(&self) -> &str {
        "Manage the email address book — add, list, search, or remove contacts"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
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
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let action = match input["action"].as_str() {
            Some(a) => a,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'action'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let mut contacts = self.contacts.lock().await;

        match action {
            "add" => {
                let name = match input["name"].as_str() {
                    Some(n) if !n.trim().is_empty() => n.trim().to_string(),
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing required parameter 'name' for add",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };
                let email = match input["email"].as_str() {
                    Some(e) if !e.trim().is_empty() => e.trim().to_lowercase(),
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing required parameter 'email' for add",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };
                contacts.add(Contact {
                    name: name.clone(),
                    email: email.clone(),
                });
                let output = serde_json::json!({
                    "success": true,
                    "action": "add",
                    "contact": { "name": name, "email": email },
                });
                Ok(ToolResult::success(
                    output,
                    start.elapsed().as_millis() as u64,
                ))
            }
            "list" => {
                let list: Vec<_> = contacts
                    .list()
                    .iter()
                    .map(|c| serde_json::json!({"name": c.name, "email": c.email}))
                    .collect();
                let output = serde_json::json!({
                    "contacts": list,
                    "count": list.len(),
                });
                Ok(ToolResult::success(
                    output,
                    start.elapsed().as_millis() as u64,
                ))
            }
            "search" => {
                let query = input["query"].as_str().unwrap_or("");
                let results: Vec<_> = contacts
                    .search(query)
                    .iter()
                    .map(|c| serde_json::json!({"name": c.name, "email": c.email}))
                    .collect();
                let output = serde_json::json!({
                    "results": results,
                    "count": results.len(),
                    "query": query,
                });
                Ok(ToolResult::success(
                    output,
                    start.elapsed().as_millis() as u64,
                ))
            }
            "remove" => {
                let email = match input["email"].as_str() {
                    Some(e) if !e.trim().is_empty() => e.trim(),
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing required parameter 'email' for remove",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };
                let removed = contacts.remove(email);
                let output = serde_json::json!({
                    "success": removed,
                    "action": "remove",
                    "email": email,
                });
                Ok(ToolResult::success(
                    output,
                    start.elapsed().as_millis() as u64,
                ))
            }
            _ => Ok(ToolResult::failure(
                format!("Unknown action '{action}' — use add, list, search, or remove"),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

// ── email.rules ─────────────────────────────────────────────────────

pub struct EmailRulesTool {
    rules: RulesStore,
}

impl EmailRulesTool {
    pub fn new(rules: RulesStore) -> Self {
        Self { rules }
    }
}

#[async_trait]
impl Tool for EmailRulesTool {
    fn name(&self) -> &str {
        "email.rules"
    }

    fn description(&self) -> &str {
        "Manage pre-approval rules for auto-sending emails"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
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
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();

        let action = match input["action"].as_str() {
            Some(a) => a,
            None => {
                return Ok(ToolResult::failure(
                    "Missing required parameter 'action'",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let mut rules = self.rules.lock().await;

        match action {
            "add" => {
                let id = match input["id"].as_str() {
                    Some(id) if !id.trim().is_empty() => id.trim().to_string(),
                    _ => uuid::Uuid::new_v4().to_string(),
                };
                let to = match input["to"].as_str() {
                    Some(t) if !t.trim().is_empty() => t.trim().to_string(),
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing required parameter 'to' for add",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };
                let subject_pattern = input["subject_pattern"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string());

                if let Some(ref pat) = subject_pattern {
                    if regex::Regex::new(pat).is_err() {
                        return Ok(ToolResult::failure(
                            format!("Invalid regex in subject_pattern: '{pat}'"),
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                }

                let enabled = input["enabled"].as_bool().unwrap_or(true);
                let rule = PreApprovalRule {
                    id: id.clone(),
                    to: to.clone(),
                    subject_pattern: subject_pattern.clone(),
                    enabled,
                };
                rules.add(rule);
                let output = serde_json::json!({
                    "success": true,
                    "action": "add",
                    "rule": {
                        "id": id,
                        "to": to,
                        "subject_pattern": subject_pattern,
                        "enabled": enabled,
                    },
                });
                Ok(ToolResult::success(
                    output,
                    start.elapsed().as_millis() as u64,
                ))
            }
            "list" => {
                let list: Vec<_> = rules
                    .list()
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "to": r.to,
                            "subject_pattern": r.subject_pattern,
                            "enabled": r.enabled,
                        })
                    })
                    .collect();
                let output = serde_json::json!({
                    "rules": list,
                    "count": list.len(),
                });
                Ok(ToolResult::success(
                    output,
                    start.elapsed().as_millis() as u64,
                ))
            }
            "remove" => {
                let id = match input["id"].as_str() {
                    Some(id) if !id.trim().is_empty() => id.trim(),
                    _ => {
                        return Ok(ToolResult::failure(
                            "Missing required parameter 'id' for remove",
                            start.elapsed().as_millis() as u64,
                        ));
                    }
                };
                let removed = rules.remove(id);
                let output = serde_json::json!({
                    "success": removed,
                    "action": "remove",
                    "id": id,
                });
                Ok(ToolResult::success(
                    output,
                    start.elapsed().as_millis() as u64,
                ))
            }
            _ => Ok(ToolResult::failure(
                format!("Unknown action '{action}' — use add, list, or remove"),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_provider() {
        assert_eq!(infer_provider("imap.gmail.com"), "Gmail");
        assert_eq!(infer_provider("outlook.office365.com"), "Outlook");
        assert_eq!(infer_provider("imap.mail.yahoo.com"), "Yahoo");
        assert_eq!(infer_provider("imap.mail.me.com"), "iCloud");
        assert_eq!(infer_provider("imap.zoho.com"), "Zoho");
        assert_eq!(infer_provider("imap.fastmail.com"), "Fastmail");
        assert_eq!(infer_provider("imap.protonmail.ch"), "ProtonMail");
        assert_eq!(infer_provider("imap.example.com"), "IMAP");
    }

    #[test]
    fn test_friendly_folder_name() {
        assert_eq!(friendly_folder_name("[Gmail]/Sent Mail"), "Sent");
        assert_eq!(friendly_folder_name("[Gmail]/Trash"), "Trash");
        assert_eq!(friendly_folder_name("[Gmail]/Spam"), "Spam");
        assert_eq!(friendly_folder_name("Sent Items"), "Sent");
        assert_eq!(friendly_folder_name("Deleted Items"), "Trash");
        assert_eq!(friendly_folder_name("Junk Email"), "Spam");
        assert_eq!(friendly_folder_name("INBOX"), "Inbox");
        assert_eq!(friendly_folder_name("Drafts"), "Drafts");
        assert_eq!(friendly_folder_name("[Gmail]/Custom"), "Custom");
        assert_eq!(friendly_folder_name("INBOX.Subfolder"), "Subfolder");
        assert_eq!(friendly_folder_name("My Custom Folder"), "My Custom Folder");
    }

    #[test]
    fn test_text_preview() {
        assert_eq!(text_preview("Short text", 100), "Short text");
        let long = "a ".repeat(100);
        let preview = text_preview(&long, 100);
        assert!(preview.len() <= 102);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn test_text_preview_whitespace_normalization() {
        let text = "Hello\n\nWorld\t\there";
        let preview = text_preview(text, 100);
        assert_eq!(preview, "Hello  World  here");
    }

    #[test]
    fn test_truncate_body() {
        let short = "Hello world";
        assert_eq!(truncate_body(short), "Hello world");

        let long = "x".repeat(MAX_BODY_CHARS + 100);
        let result = truncate_body(&long);
        assert!(result.contains("[truncated"));
        assert!(result.contains(&format!("{}", MAX_BODY_CHARS + 100)));
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1_572_864), "1.5 MB");
    }

    #[test]
    fn test_is_unread() {
        assert!(is_unread_flags(&[]));
        assert!(is_unread_flags(&["Flagged".to_string()]));
        assert!(!is_unread_flags(&["Seen".to_string()]));
        assert!(!is_unread_flags(&["Flag(Seen)".to_string()]));
    }

    #[test]
    fn test_parse_mime_simple_text() {
        let raw = b"From: test@example.com\r\nTo: me@example.com\r\nSubject: Test\r\nContent-Type: text/plain\r\n\r\nHello World";
        let (text, markdown, attachments) = parse_mime_body(raw);
        assert!(text.contains("Hello World"));
        assert!(markdown.is_empty());
        assert!(attachments.is_empty());
    }

    #[test]
    fn test_parse_mime_html_only() {
        let raw =
            b"From: test@example.com\r\nContent-Type: text/html\r\n\r\n<h1>Hello</h1><p>World</p>";
        let (text, markdown, attachments) = parse_mime_body(raw);
        assert!(text.is_empty());
        assert!(!markdown.is_empty());
        assert!(markdown.contains("Hello"));
        assert!(attachments.is_empty());
    }

    #[test]
    fn test_parse_mime_multipart() {
        let raw = b"Content-Type: multipart/alternative; boundary=\"boundary123\"\r\n\r\n--boundary123\r\nContent-Type: text/plain\r\n\r\nPlain text version\r\n--boundary123\r\nContent-Type: text/html\r\n\r\n<p>HTML version</p>\r\n--boundary123--";
        let (text, markdown, attachments) = parse_mime_body(raw);
        assert!(text.contains("Plain text version"));
        assert!(markdown.contains("HTML version"));
        assert!(attachments.is_empty());
    }

    #[test]
    fn test_format_single_address() {
        let name = Some(Cow::Borrowed(b"John Doe" as &[u8]));
        let mailbox = Some(Cow::Borrowed(b"john" as &[u8]));
        let host = Some(Cow::Borrowed(b"example.com" as &[u8]));
        assert_eq!(
            format_single_address(&name, &mailbox, &host),
            "John Doe <john@example.com>"
        );

        let no_name: Option<Cow<[u8]>> = None;
        assert_eq!(
            format_single_address(&no_name, &mailbox, &host),
            "john@example.com"
        );
    }

    // ── email.search unit tests ──────────────────────────────────────

    #[test]
    fn test_parse_since_date_relative() {
        // "7d" should produce a date 7 days ago in dd-Mon-yyyy format
        let result = parse_since_date("7d").unwrap();
        let expected = (chrono::Utc::now().date_naive() - chrono::Duration::days(7))
            .format("%d-%b-%Y")
            .to_string();
        assert_eq!(result, expected);

        // "30d"
        let result = parse_since_date("30d").unwrap();
        let expected = (chrono::Utc::now().date_naive() - chrono::Duration::days(30))
            .format("%d-%b-%Y")
            .to_string();
        assert_eq!(result, expected);

        // "1d"
        let result = parse_since_date("1d").unwrap();
        let expected = (chrono::Utc::now().date_naive() - chrono::Duration::days(1))
            .format("%d-%b-%Y")
            .to_string();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_parse_since_date_iso() {
        let result = parse_since_date("2026-01-15").unwrap();
        assert_eq!(result, "15-Jan-2026");

        let result = parse_since_date("2026-03-05").unwrap();
        assert_eq!(result, "05-Mar-2026");
    }

    #[test]
    fn test_parse_since_date_invalid() {
        assert!(parse_since_date("garbage").is_err());
        assert!(parse_since_date("").is_err());
        assert!(parse_since_date("abc123").is_err());
        assert!(parse_since_date("0d").is_err()); // 0 days is not valid
        assert!(parse_since_date("-5d").is_err());
    }

    #[test]
    fn test_build_search_criteria_text_only() {
        let result = build_search_criteria(Some("hello"), None, None, false).unwrap();
        assert_eq!(result, "TEXT \"hello\"");
    }

    #[test]
    fn test_build_search_criteria_combined() {
        let result =
            build_search_criteria(Some("hello"), Some("2026-03-01"), Some("bob@x.com"), true)
                .unwrap();
        assert!(result.contains("TEXT \"hello\""));
        assert!(result.contains("SINCE 01-Mar-2026"));
        assert!(result.contains("FROM \"bob@x.com\""));
        assert!(result.contains("UNSEEN"));
    }

    #[test]
    fn test_build_search_criteria_filters_only() {
        // No query, just since + from should work
        let result =
            build_search_criteria(None, Some("7d"), Some("alice@example.com"), false).unwrap();
        assert!(result.contains("SINCE"));
        assert!(result.contains("FROM \"alice@example.com\""));
        assert!(!result.contains("TEXT"));
    }

    #[test]
    fn test_build_search_criteria_empty() {
        // No query and no filters → error
        let result = build_search_criteria(None, None, None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No search criteria provided"));
    }

    #[test]
    fn test_build_search_criteria_unread_only() {
        let result = build_search_criteria(None, None, None, true).unwrap();
        assert_eq!(result, "UNSEEN");
    }

    #[test]
    fn test_build_search_criteria_escapes_quotes() {
        let result = build_search_criteria(Some("hello \"world\""), None, None, false).unwrap();
        assert_eq!(result, "TEXT \"hello \\\"world\\\"\"");
    }

    // ── email.send unit tests ────────────────────────────────────────

    #[test]
    fn test_parse_and_validate_addresses_single() {
        let result = parse_and_validate_addresses("user@example.com").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].email.to_string(), "user@example.com");
    }

    #[test]
    fn test_parse_and_validate_addresses_multiple() {
        let result = parse_and_validate_addresses("alice@example.com, bob@example.com").unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_parse_and_validate_addresses_with_name() {
        let result = parse_and_validate_addresses("Alice <alice@example.com>").unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].email.to_string(), "alice@example.com");
        assert_eq!(result[0].name.as_deref(), Some("Alice"));
    }

    #[test]
    fn test_parse_and_validate_addresses_invalid() {
        let result = parse_and_validate_addresses("not-an-email");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_and_validate_addresses_empty() {
        let result = parse_and_validate_addresses("").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_reply_subject_prefix() {
        assert_eq!(reply_subject("Hello"), "Re: Hello");
        assert_eq!(reply_subject("Re: Hello"), "Re: Hello");
        assert_eq!(reply_subject("RE: Hello"), "RE: Hello");
        assert_eq!(
            reply_subject("re: already prefixed"),
            "re: already prefixed"
        );
        assert_eq!(reply_subject("  Re: with spaces  "), "Re: with spaces");
    }

    #[test]
    fn test_sent_folder_detection() {
        assert_eq!(sent_folder_name("imap.gmail.com"), "[Gmail]/Sent Mail");
        assert_eq!(sent_folder_name("imap.mail.me.com"), "Sent Messages");
        assert_eq!(sent_folder_name("imap.icloud.com"), "Sent Messages");
        assert_eq!(sent_folder_name("imap.example.com"), "Sent");
        assert_eq!(sent_folder_name("imap.outlook.com"), "Sent");
        assert_eq!(sent_folder_name("imap.fastmail.com"), "Sent");
    }

    #[test]
    fn test_build_from_mailbox_simple() {
        let config = super::super::config::EmailAccountConfig {
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            username: "user@example.com".into(),
            password: "secret".into(),
            display_name: None,
            tls: true,
            connect_timeout_secs: 30,
            command_timeout_secs: 60,
        };
        let mbox = build_from_mailbox(&config).unwrap();
        assert_eq!(mbox.email.to_string(), "user@example.com");
        assert!(mbox.name.is_none());
    }

    #[test]
    fn test_build_from_mailbox_with_display_name() {
        let config = super::super::config::EmailAccountConfig {
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            username: "user@example.com".into(),
            password: "secret".into(),
            display_name: Some("Test User".into()),
            tls: true,
            connect_timeout_secs: 30,
            command_timeout_secs: 60,
        };
        let mbox = build_from_mailbox(&config).unwrap();
        assert_eq!(mbox.email.to_string(), "user@example.com");
        assert_eq!(mbox.name.as_deref(), Some("Test User"));
    }

    // ── Provider folder mapping tests ────────────────────────────────

    #[test]
    fn test_provider_folder_gmail() {
        assert_eq!(
            provider_folder("imap.gmail.com", LogicalFolder::Archive),
            "[Gmail]/All Mail"
        );
        assert_eq!(
            provider_folder("imap.gmail.com", LogicalFolder::Trash),
            "[Gmail]/Trash"
        );
        assert_eq!(
            provider_folder("imap.gmail.com", LogicalFolder::Sent),
            "[Gmail]/Sent Mail"
        );
    }

    #[test]
    fn test_provider_folder_icloud() {
        assert_eq!(
            provider_folder("imap.mail.me.com", LogicalFolder::Archive),
            "Archive"
        );
        assert_eq!(
            provider_folder("imap.mail.me.com", LogicalFolder::Trash),
            "Deleted Messages"
        );
        assert_eq!(
            provider_folder("imap.mail.me.com", LogicalFolder::Sent),
            "Sent Messages"
        );
    }

    #[test]
    fn test_provider_folder_outlook() {
        assert_eq!(
            provider_folder("outlook.office365.com", LogicalFolder::Archive),
            "Archive"
        );
        assert_eq!(
            provider_folder("outlook.office365.com", LogicalFolder::Trash),
            "Trash"
        );
    }

    #[test]
    fn test_provider_folder_generic() {
        assert_eq!(
            provider_folder("imap.example.com", LogicalFolder::Archive),
            "Archive"
        );
        assert_eq!(
            provider_folder("imap.example.com", LogicalFolder::Trash),
            "Trash"
        );
        assert_eq!(
            provider_folder("imap.example.com", LogicalFolder::Sent),
            "Sent"
        );
    }

    // ── parse_uids tests ─────────────────────────────────────────────

    #[test]
    fn test_parse_uids_single() {
        let input = serde_json::json!({"uid": 42});
        assert_eq!(parse_uids(&input).unwrap(), "42");
    }

    #[test]
    fn test_parse_uids_bulk() {
        let input = serde_json::json!({"uids": [1, 2, 3]});
        assert_eq!(parse_uids(&input).unwrap(), "1,2,3");
    }

    #[test]
    fn test_parse_uids_missing() {
        let input = serde_json::json!({"account": "test"});
        assert!(parse_uids(&input).is_err());
        assert!(parse_uids(&input)
            .unwrap_err()
            .contains("Missing required parameter"));
    }

    #[test]
    fn test_parse_uids_empty_array() {
        let input = serde_json::json!({"uids": []});
        assert!(parse_uids(&input).is_err());
        assert!(parse_uids(&input)
            .unwrap_err()
            .contains("must not be empty"));
    }

    #[test]
    fn test_parse_uids_zero() {
        let input = serde_json::json!({"uid": 0});
        assert!(parse_uids(&input).is_err());
    }

    // ── parse_mark_action tests ──────────────────────────────────────

    #[test]
    fn test_mark_action_valid() {
        assert_eq!(parse_mark_action("read").unwrap(), "+FLAGS (\\Seen)");
        assert_eq!(parse_mark_action("unread").unwrap(), "-FLAGS (\\Seen)");
        assert_eq!(parse_mark_action("flag").unwrap(), "+FLAGS (\\Flagged)");
        assert_eq!(parse_mark_action("unflag").unwrap(), "-FLAGS (\\Flagged)");
    }

    #[test]
    fn test_mark_action_invalid() {
        let result = parse_mark_action("spam");
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(err_msg.contains("Invalid action"));
        assert!(err_msg.contains("read, unread, flag, unflag"));
    }

    // ── email.forward unit tests ─────────────────────────────────────

    #[test]
    fn test_forward_subject_no_prefix() {
        assert_eq!(forward_subject("Meeting Notes"), "Fwd: Meeting Notes");
    }

    #[test]
    fn test_forward_subject_already_prefixed() {
        assert_eq!(forward_subject("Fwd: Meeting Notes"), "Fwd: Meeting Notes");
    }

    #[test]
    fn test_forward_subject_case_insensitive_prefix() {
        assert_eq!(forward_subject("FWD: Meeting Notes"), "FWD: Meeting Notes");
        assert_eq!(forward_subject("fwd: Meeting Notes"), "fwd: Meeting Notes");
    }

    #[test]
    fn test_forward_subject_trims_whitespace() {
        assert_eq!(forward_subject("  Hello  "), "Fwd: Hello");
    }

    #[test]
    fn test_forward_subject_empty() {
        assert_eq!(forward_subject(""), "Fwd: ");
    }

    fn walk(raw: &[u8]) -> MimeContent {
        walk_mime(&mailparse::parse_mail(raw).unwrap())
    }

    fn summary(content: &MimeContent) -> Vec<(&str, &str, &str, bool)> {
        content
            .parts
            .iter()
            .map(|p| {
                (
                    p.part.as_str(),
                    p.filename.as_str(),
                    p.mime_type.as_str(),
                    p.inline,
                )
            })
            .collect()
    }

    #[test]
    fn walk_mime_plain_text_has_no_parts() {
        let content = walk(b"Content-Type: text/plain\r\n\r\nHello World");
        assert_eq!(content.text, "Hello World");
        assert!(content.html.is_empty());
        assert!(content.parts.is_empty());
    }

    #[test]
    fn walk_mime_html_only() {
        let content = walk(b"Content-Type: text/html\r\n\r\n<p>Hello</p>");
        assert!(content.text.is_empty());
        assert!(content.html.contains("<p>Hello</p>"));
        assert!(content.parts.is_empty());
    }

    #[test]
    fn walk_mime_flat_multipart_paths_and_classification() {
        let content = walk(
            b"Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
              --b\r\nContent-Type: text/plain\r\n\r\nBody text\r\n\
              --b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"doc.pdf\"\r\n\r\npdf_data\r\n\
              --b\r\nContent-Type: image/png; name=\"logo.png\"\r\n\r\npng_data\r\n\
              --b--",
        );
        assert_eq!(content.text.trim_end(), "Body text");
        assert_eq!(
            summary(&content),
            vec![
                ("2", "doc.pdf", "application/pdf", false),
                ("3", "logo.png", "image/png", true),
            ]
        );
    }

    #[test]
    fn walk_mime_nested_multipart_paths_and_body_concatenation() {
        let content = walk(
            b"Content-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n\
              --outer\r\nContent-Type: multipart/alternative; boundary=\"inner\"\r\n\r\n\
              --inner\r\nContent-Type: text/plain\r\n\r\nFirst\r\n\
              --inner\r\nContent-Type: text/html\r\n\r\n<p>First</p>\r\n\
              --inner--\r\n\
              --outer\r\nContent-Type: text/plain\r\n\r\nSecond\r\n\
              --outer\r\nContent-Type: application/vnd.openxmlformats-officedocument.wordprocessingml.document\r\n\
              Content-Disposition: attachment; filename=\"spec.docx\"\r\n\r\ndocx\r\n\
              --outer--",
        );
        assert_eq!(content.text, "First\r\n\n\nSecond\r\n");
        assert_eq!(content.html.trim_end(), "<p>First</p>");
        assert_eq!(
            summary(&content),
            vec![(
                "3",
                "spec.docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                false
            )]
        );
    }

    #[test]
    fn walk_mime_addresses_grandchildren_with_dotted_paths() {
        let content = walk(
            b"Content-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n\
              --outer\r\nContent-Type: text/plain\r\n\r\nBody\r\n\
              --outer\r\nContent-Type: multipart/related; boundary=\"inner\"\r\n\r\n\
              --inner\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:x\">\r\n\
              --inner\r\nContent-Type: image/gif; name=\"x.gif\"\r\n\r\ngif\r\n\
              --inner--\r\n\
              --outer--",
        );
        assert_eq!(summary(&content), vec![("2.2", "x.gif", "image/gif", true)]);
    }

    #[test]
    fn walk_mime_single_part_attachment_is_the_root() {
        let content = walk(
            b"Content-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"only.pdf\"\r\n\r\npdf",
        );
        assert_eq!(
            summary(&content),
            vec![("0", "only.pdf", "application/pdf", false)]
        );
    }

    #[test]
    fn walk_mime_multipart_with_attachment_disposition_stays_one_part() {
        let content = walk(
            b"Content-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n\
              --outer\r\nContent-Type: multipart/mixed; boundary=\"inner\"\r\n\
              Content-Disposition: attachment; filename=\"bundle\"\r\n\r\n\
              --inner\r\nContent-Type: text/plain\r\n\r\nhidden\r\n\
              --inner--\r\n\
              --outer--",
        );
        assert!(content.text.is_empty());
        assert_eq!(
            summary(&content),
            vec![("1", "bundle", "multipart/mixed", false)]
        );
    }

    #[test]
    fn walk_mime_lists_empty_attachments_but_not_empty_inline_parts() {
        let content = walk(
            b"Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
              --b\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=\"empty.txt\"\r\n\r\n\
              --b\r\nContent-Type: image/png\r\n\r\n\
              --b--",
        );
        assert_eq!(
            summary(&content),
            vec![("1", "empty.txt", "text/plain", false)]
        );
        assert!(content.parts[0].data.is_empty());
    }

    #[test]
    fn walk_mime_decodes_base64_parts_to_original_bytes() {
        let content = walk(
            b"Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
              --b\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\
              Content-Disposition: attachment; filename=\"blob.bin\"\r\n\r\nAAEC/w==\r\n\
              --b--",
        );
        assert_eq!(content.parts[0].data, vec![0x00, 0x01, 0x02, 0xff]);
    }

    #[test]
    fn walk_mime_filename_sources_and_fallback() {
        let filename = |headers: &str| {
            let raw = format!("{headers}\r\n\r\ndata");
            walk(raw.as_bytes()).parts.remove(0).filename
        };
        assert_eq!(
            filename("Content-Type: application/pdf; name=\"ct.pdf\"\r\nContent-Disposition: attachment; filename=\"disp.pdf\""),
            "disp.pdf",
            "disposition filename wins"
        );
        assert_eq!(
            filename(
                "Content-Type: application/pdf; name=\"ct.pdf\"\r\nContent-Disposition: attachment"
            ),
            "ct.pdf"
        );
        assert_eq!(
            filename("Content-Type: application/pdf\r\nContent-Disposition: attachment"),
            "unnamed"
        );
        assert_eq!(
            filename("Content-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"Q1; final.pdf\"; size=4"),
            "Q1; final.pdf",
            "quoted ';' survives"
        );
        assert_eq!(
            filename("Content-Type: application/pdf; name=\"a \\\"b\\\"; c.pdf\"\r\nContent-Disposition: attachment"),
            "a \"b\"; c.pdf",
            "escaped quotes survive"
        );
        assert_eq!(
            filename("Content-Type: application/pdf\r\nContent-Disposition: attachment; filename*=UTF-8''%E2%82%AC%20rates.pdf"),
            "€ rates.pdf",
            "RFC 2231"
        );
        assert_eq!(
            filename("Content-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"=?UTF-8?B?w6l0w6kucGRm?=\""),
            "été.pdf",
            "RFC 2047"
        );
    }

    #[test]
    fn quoted_param_matches_whole_keys_only() {
        assert_eq!(
            quoted_param("image/png; filename=\"f;1.png\"; name=\"n;2.png\"", "name"),
            Some("n;2.png".to_string())
        );
        assert_eq!(
            quoted_param("image/png; filename=\"f;1.png\"", "name"),
            None
        );
        assert_eq!(
            quoted_param("attachment; filename=\"unterminated", "filename"),
            None
        );
    }

    #[test]
    fn attachment_summary_carries_part_and_inline_only_when_inline() {
        let part = |inline| MimePart {
            part: "2.1".into(),
            filename: "a.pdf".into(),
            mime_type: "application/pdf".into(),
            inline,
            data: vec![0; 2048],
        };
        assert_eq!(
            attachment_summary(&part(false)),
            serde_json::json!({
                "part": "2.1",
                "filename": "a.pdf",
                "mime_type": "application/pdf",
                "size": format_size(2048),
                "size_bytes": 2048,
            })
        );
        assert_eq!(attachment_summary(&part(true))["inline"], true);
    }

    #[test]
    fn parse_mime_body_lists_attachments_with_part() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
                    --b\r\nContent-Type: text/plain\r\n\r\nBody\r\n\
                    --b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"doc.pdf\"\r\n\r\npdf_data\r\n\
                    --b--";
        let (text, _, attachments) = parse_mime_body(raw);
        assert!(text.contains("Body"));
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0]["part"], "2");
        assert_eq!(attachments[0]["filename"], "doc.pdf");
        assert!(attachments[0].get("inline").is_none());
    }

    #[test]
    fn forward_reattaches_attachment_parts_only() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
              --b\r\nContent-Type: text/plain\r\n\r\nBody\r\n\
              --b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"doc.pdf\"\r\n\r\npdf\r\n\
              --b\r\nContent-Type: image/png; name=\"logo.png\"\r\n\r\npng\r\n\
              --b--";
        let names = |include| {
            forwardable_attachments(walk(raw).parts, include)
                .into_iter()
                .map(|p| p.filename)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(true), vec!["doc.pdf"]);
        assert!(names(false).is_empty());
    }

    #[test]
    fn listed_part_resolves_to_the_same_file_on_a_rewalk() {
        let raw = b"Content-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n\
              --outer\r\nContent-Type: text/plain\r\n\r\nBody\r\n\
              --outer\r\nContent-Type: multipart/mixed; boundary=\"inner\"\r\n\r\n\
              --inner\r\nContent-Type: text/plain\r\n\r\nNested\r\n\
              --inner\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\
              Content-Disposition: attachment; filename=\"deep.bin\"\r\n\r\nAAEC/w==\r\n\
              --inner--\r\n\
              --outer--";
        let (_, _, listing) = parse_mime_body(raw);
        let part = listing[0]["part"].as_str().unwrap();
        assert_eq!(part, "2.2");
        let file = select_part(walk(raw).parts, part).unwrap();
        assert_eq!(file.filename, "deep.bin");
        assert_eq!(file.data, vec![0x00, 0x01, 0x02, 0xff]);
    }

    #[test]
    fn select_part_finds_by_path_or_lists_available() {
        let part = |path: &str, name: &str| MimePart {
            part: path.into(),
            filename: name.into(),
            mime_type: "application/pdf".into(),
            inline: false,
            data: Vec::new(),
        };
        let parts = || vec![part("2", "a.pdf"), part("3.1", "b.pdf")];
        assert_eq!(select_part(parts(), "3.1").unwrap().filename, "b.pdf");
        let err = select_part(parts(), "4").err().unwrap();
        assert!(err.contains("2 (a.pdf), 3.1 (b.pdf)"), "{err}");
        let none = select_part(Vec::new(), "1").err().unwrap();
        assert!(none.contains("no attachments"), "{none}");
    }

    #[test]
    fn message_locator_validates_and_defaults_folder() {
        let input = serde_json::json!({"account": "work", "uid": "42"});
        let ok = message_locator(&input).unwrap();
        assert_eq!((ok.account, ok.uid, ok.folder), ("work", 42, "INBOX"));
        let input = serde_json::json!({"account": "w", "uid": 1, "folder": "Archive"});
        assert_eq!(message_locator(&input).unwrap().folder, "Archive");
        for bad in [
            serde_json::json!({"uid": 1}),
            serde_json::json!({"account": " ", "uid": 1}),
            serde_json::json!({"account": "w"}),
            serde_json::json!({"account": "w", "uid": 0}),
            serde_json::json!({"account": "w", "uid": u64::from(u32::MAX) + 1}),
        ] {
            assert!(message_locator(&bad).is_err(), "{bad}");
        }
    }
}
