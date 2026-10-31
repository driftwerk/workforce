//! Every table column shaped like a person's identity, in every crate's
//! Postgres migrations and inline SQLite schema, must be claimed by a
//! [`Category`] or declared not personal with a reason. A store added later
//! with an identity column fails here until it is registered.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::Category;

/// `(crate, table, column)`.
type Column = (String, String, String);

#[derive(Debug, Clone, Copy)]
enum Claim {
    By(Category),
    NotPersonal(&'static str),
}

use Category as C;
use Claim::{By, NotPersonal};

const INSTANCE: &str = "a hub instance holding a lease or claim, not a person";

const CLAIMS: &[(&str, &str, &str, Claim)] = &[
    ("common", "artifact_shares", "user_id", By(C::Artifacts)),
    ("common", "artifacts", "owner_id", By(C::Artifacts)),
    ("common", "artifacts", "created_by", By(C::AuditRecords)),
    (
        "common",
        "artifacts",
        "last_modified_by",
        By(C::AuditRecords),
    ),
    ("common", "audit_log", "actor", By(C::AuditRecords)),
    ("common", "cost_records", "identity_id", By(C::CostLedger)),
    ("common", "cost_records", "payer", By(C::CostLedger)),
    (
        "common",
        "def_revisions",
        "owner_identity_id",
        By(C::Definitions),
    ),
    ("common", "def_revisions", "edited_by", By(C::AuditRecords)),
    (
        "common",
        "def_shares",
        "owner_identity_id",
        By(C::Definitions),
    ),
    ("common", "def_shares", "principal_id", By(C::Definitions)),
    (
        "common",
        "def_shares",
        "principal_type",
        NotPersonal("whether principal_id is a user or a team"),
    ),
    ("common", "def_shares", "granted_by", By(C::AuditRecords)),
    ("common", "lesson_events", "identity_id", By(C::Signals)),
    (
        "common",
        "monitor_channel_blocks",
        "owner",
        By(C::Definitions),
    ),
    ("common", "monitor_cursors", "owner", By(C::Definitions)),
    ("common", "monitor_dispatches", "owner", By(C::Definitions)),
    (
        "common",
        "monitor_source_state",
        "owner",
        By(C::Definitions),
    ),
    (
        "common",
        "policy_proposals",
        "reviewed_by",
        By(C::AuditRecords),
    ),
    (
        "common",
        "quarantine_entries",
        "reviewed_by",
        NotPersonal("dormant: nothing in the hub writes quarantine entries"),
    ),
    (
        "common",
        "quarantine_entries",
        "sender_id",
        NotPersonal("dormant: nothing in the hub writes quarantine entries"),
    ),
    (
        "common",
        "relationship_events",
        "identity_id",
        By(C::Signals),
    ),
    (
        "common",
        "response_claims",
        "claimed_by",
        NotPersonal(INSTANCE),
    ),
    ("common", "sessions", "owner", By(C::Sessions)),
    ("common", "sessions", "turn_run_as", By(C::Sessions)),
    ("common", "sessions", "turn_bill_to", By(C::Sessions)),
    (
        "common",
        "sessions",
        "turn_principal_token",
        NotPersonal("the token of the turn a principal was written for"),
    ),
    ("common", "task_telemetry", "identity_id", By(C::CostLedger)),
    ("common", "tasks", "assignee", NotPersonal("an agent id")),
    (
        "common",
        "trigger_endpoints",
        "owner_identity_id",
        By(C::Definitions),
    ),
    (
        "common",
        "turn_claims",
        "owner_instance",
        NotPersonal(INSTANCE),
    ),
    (
        "common",
        "workflow_defs",
        "owner_identity_id",
        By(C::Definitions),
    ),
    (
        "common",
        "workflow_defs",
        "run_as_identity_id",
        By(C::Definitions),
    ),
    (
        "common",
        "workflow_defs",
        "last_modified_by",
        By(C::Definitions),
    ),
    (
        "common",
        "workflow_instances",
        "owner_instance",
        NotPersonal(INSTANCE),
    ),
    ("desk", "attachments", "owner", By(C::Sessions)),
    (
        "desk",
        "rooms",
        "default_member",
        NotPersonal("an agent, as its hub/agent key"),
    ),
    (
        "desk",
        "rooms",
        "members",
        NotPersonal("the room's agents, as hub/agent keys"),
    ),
    (
        "desk",
        "activity",
        "actor",
        NotPersonal("an agent, a session, the overseer, or \"you\" — never a person's id"),
    ),
    (
        "desk",
        "activity",
        "subject",
        NotPersonal("the agent or session an event is about"),
    ),
    (
        "desk",
        "suggestions",
        "subject",
        NotPersonal("the agent or session a suggestion is about"),
    ),
    (
        "hub-core",
        "budget_overrides",
        "scope",
        By(C::BudgetOverrides),
    ),
    (
        "hub-core",
        "budget_overrides",
        "created_by",
        By(C::AuditRecords),
    ),
    (
        "hub-core",
        "budget_override_audit",
        "actor",
        By(C::AuditRecords),
    ),
    (
        "hub-core",
        "budget_override_audit",
        "scope",
        By(C::AuditRecords),
    ),
    ("hub", "collections", "scope_kind", By(C::Documents)),
    ("hub", "collections", "scope_id", By(C::Documents)),
    ("hub", "documents", "scope_kind", By(C::Documents)),
    ("hub", "documents", "scope_id", By(C::Documents)),
    ("hub", "documents", "created_by", By(C::AuditRecords)),
    (
        "hub",
        "documents",
        "lease_owner",
        NotPersonal("the run or replica holding a claim"),
    ),
    ("hub", "documents_default", "scope_kind", By(C::Documents)),
    ("hub", "documents_default", "scope_id", By(C::Documents)),
    (
        "hub",
        "documents_default",
        "created_by",
        By(C::AuditRecords),
    ),
    (
        "hub",
        "documents_default",
        "lease_owner",
        NotPersonal("the run or replica holding a claim"),
    ),
    ("hub", "column_people", "person", By(C::KnowHow)),
    ("hub", "identity_pictures", "identity_id", By(C::Pictures)),
    ("hub", "intents", "person", By(C::KnowHow)),
    ("hub", "provenance", "person", By(C::KnowHow)),
    ("hub", "records", "person", By(C::DecisionRecords)),
    ("hub", "interventions", "person", By(C::DecisionRecords)),
    ("hub", "interventions", "actor", By(C::DecisionRecords)),
    ("hub", "pending_approvals", "requester_id", By(C::Approvals)),
    ("hub", "turns", "owner", By(C::Signals)),
    (
        "hub",
        "turns",
        "subject",
        NotPersonal("the agent or Desk session a turn is of"),
    ),
    (
        "hub",
        "turns",
        "subject_doc",
        NotPersonal("the agent or Desk session a turn is of"),
    ),
    ("hub", "plan_readings", "owner", By(C::Signals)),
    ("hub", "gists", "owner", By(C::Signals)),
    (
        "hub",
        "gists",
        "subject",
        NotPersonal("the agent or Desk session a gist is of"),
    ),
    ("hub", "summaries", "owner", By(C::Signals)),
    ("hub", "allows", "person", By(C::PermissionRules)),
    ("hub", "presence", "identity_id", By(C::Devices)),
    (
        "hub",
        "pending_approvals",
        "resolved_by",
        By(C::AuditRecords),
    ),
    (
        "hub",
        "pending_approval_targets",
        "approver_id",
        By(C::Approvals),
    ),
    (
        "hub",
        "pending_approval_targets",
        "slack_user_id",
        By(C::Approvals),
    ),
    ("hub-core", "vault_entries", "scope", By(C::Vault)),
    ("hub-core", "vault_audit", "scope", By(C::AuditRecords)),
    (
        "hub-core",
        "vault_scopes",
        "scope",
        NotPersonal("which vault: global or an agent's"),
    ),
    (
        "identity",
        "identity_lifecycle_ops",
        "subject",
        By(C::AuditRecords),
    ),
    (
        "identity",
        "identity_lifecycle_ops",
        "actor",
        By(C::AuditRecords),
    ),
    (
        "identity",
        "remote_mcp_tokens",
        "identity_id",
        By(C::RemoteMcpTokens),
    ),
    ("identity", "teams", "creator_id", By(C::Teams)),
    ("identity", "teams", "members", By(C::Teams)),
    ("memory", "memories", "identity_id", By(C::Memories)),
    (
        "memory",
        "memories",
        "owner",
        NotPersonal("the agent the memory belongs to"),
    ),
    (
        "orchestrator",
        "workflow_defs",
        "owner_identity_id",
        By(C::Definitions),
    ),
    (
        "pairing",
        "channel_allowlist",
        "sender_id",
        By(C::ChannelAllowlist),
    ),
    (
        "pairing",
        "channel_allowlist",
        "sender_name",
        By(C::ChannelAllowlist),
    ),
    (
        "pairing",
        "channel_allowlist",
        "added_by",
        By(C::AuditRecords),
    ),
    (
        "pairing",
        "channel_pairing_requests",
        "sender_id",
        By(C::RetentionBound),
    ),
    (
        "pairing",
        "channel_pairing_requests",
        "sender_name",
        By(C::RetentionBound),
    ),
    (
        "pairing",
        "channel_pairing_requests",
        "approved_by",
        By(C::AuditRecords),
    ),
    (
        "pairing",
        "device_pairing_requests",
        "identity_hint",
        By(C::RetentionBound),
    ),
    (
        "pairing",
        "device_pairing_requests",
        "approved_by",
        By(C::AuditRecords),
    ),
    (
        "pairing",
        "node_pairing_requests",
        "approved_by",
        By(C::AuditRecords),
    ),
    (
        "permissions",
        "identity_role_membership",
        "identity_id",
        By(C::RoleGrants),
    ),
    (
        "permissions",
        "identity_role_membership",
        "granted_by",
        By(C::AuditRecords),
    ),
    (
        "permissions",
        "permission_rules",
        "scope_type",
        By(C::PermissionRules),
    ),
    (
        "permissions",
        "permission_rules",
        "scope_id",
        By(C::PermissionRules),
    ),
    (
        "permissions",
        "permission_rules",
        "created_by",
        By(C::AuditRecords),
    ),
    (
        "permissions",
        "capability_override",
        "updated_by",
        By(C::AuditRecords),
    ),
    (
        "permissions",
        "capability_override_audit",
        "actor_id",
        By(C::AuditRecords),
    ),
    (
        "permissions",
        "custom_role",
        "created_by",
        By(C::AuditRecords),
    ),
    (
        "self-learning",
        "self_learning_items",
        "created_by",
        By(C::AuditRecords),
    ),
    (
        "self-learning",
        "self_learning_items",
        "reviewed_by",
        By(C::AuditRecords),
    ),
    (
        "self-learning",
        "self_learning_items",
        "superseded_by",
        NotPersonal("the item that replaced this one"),
    ),
    (
        "self-learning",
        "deep_nodes",
        "created_by",
        By(C::AuditRecords),
    ),
    (
        "self-learning",
        "deep_nodes",
        "reviewed_by",
        By(C::AuditRecords),
    ),
    (
        "self-learning",
        "deep_nodes",
        "superseded_by",
        NotPersonal("the node that replaced this one"),
    ),
    (
        "training",
        "training_corpora",
        "scope_kind",
        By(C::TrainingCorpora),
    ),
    (
        "training",
        "training_corpora",
        "scope_id",
        By(C::TrainingCorpora),
    ),
    (
        "training",
        "training_corpora",
        "created_by",
        By(C::AuditRecords),
    ),
    (
        "training",
        "training_documents",
        "uploaded_by",
        By(C::AuditRecords),
    ),
];

fn workspace_crates() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .to_path_buf()
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target" && name != "node_modules" {
                files(&path, out);
            }
        } else if path
            .extension()
            .is_some_and(|ext| ext == "sql" || ext == "rs")
        {
            out.push(path);
        }
    }
}

/// Whether a column name reads as naming a person.
fn identity_shaped(column: &str) -> bool {
    const WORDS: [&str; 19] = [
        "owner",
        "person",
        "user",
        "identity",
        "creator",
        "requester",
        "approver",
        "principal",
        "member",
        "members",
        "sender",
        "payer",
        "actor",
        "assignee",
        "reviewer",
        "editor",
        "operator",
        "subject",
        "scope",
    ];
    let parts: Vec<&str> = column.split('_').collect();
    column.ends_with("_by")
        || column == "run_as"
        || column.contains("run_as")
        || column.contains("bill_to")
        || parts.iter().any(|part| WORDS.contains(part))
}

/// The identity-shaped columns `sql` declares: in `CREATE TABLE` bodies and
/// `ALTER TABLE … ADD COLUMN`.
fn declared(crate_name: &str, sql: &str) -> Vec<Column> {
    let lower = sql.to_lowercase();
    let mut found = Vec::new();
    let mut rest = lower.as_str();
    while let Some(at) = rest.find("create table") {
        rest = &rest[at + "create table".len()..];
        let header = rest
            .trim_start()
            .trim_start_matches("if not exists")
            .trim_start();
        let table = header
            .split(|c: char| c.is_whitespace() || c == '(')
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("")
            .trim_matches('"')
            .to_string();
        let Some(open) = header.find('(') else {
            continue;
        };
        let mut depth = 0usize;
        let mut end = open;
        for (i, ch) in header[open..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        for line in header[open + 1..end].split([',', '\n']) {
            let column = line
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches('"');
            let keyword = matches!(
                column,
                "primary" | "foreign" | "unique" | "constraint" | "check" | "references" | ""
            );
            if !keyword
                && column
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
                && identity_shaped(column)
            {
                found.push((crate_name.to_string(), table.clone(), column.to_string()));
            }
        }
    }
    let mut rest = lower.as_str();
    while let Some(at) = rest.find("alter table") {
        rest = &rest[at + "alter table".len()..];
        let statement = rest.split(';').next().unwrap_or("");
        let table = statement
            .split_whitespace()
            .find(|w| *w != "if" && *w != "exists" && *w != "only")
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_string();
        for clause in statement.split("add column").skip(1) {
            let column = clause
                .split_whitespace()
                .find(|w| !matches!(*w, "if" | "not" | "exists"))
                .unwrap_or("")
                .trim_matches('"');
            if identity_shaped(column) {
                found.push((crate_name.to_string(), table.clone(), column.to_string()));
            }
        }
    }
    found
}

fn scan() -> BTreeSet<Column> {
    let root = workspace_crates();
    let mut paths = Vec::new();
    files(&root, &mut paths);
    paths
        .into_iter()
        .filter(|path| !path.ends_with("identity_lifecycle/schema_guard.rs"))
        .flat_map(|path| {
            let crate_name = path
                .strip_prefix(&root)
                .ok()
                .and_then(|p| p.components().next())
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .unwrap_or_default();
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            declared(&crate_name, &text)
        })
        .collect()
}

#[test]
fn the_scanner_finds_the_columns_it_must() {
    let found = scan();
    for (krate, table, column) in [
        ("common", "sessions", "owner"),
        ("common", "workflow_defs", "owner_identity_id"),
        ("hub", "pending_approval_targets", "approver_id"),
        ("identity", "remote_mcp_tokens", "identity_id"),
        ("memory", "memories", "identity_id"),
        ("common", "cost_records", "identity_id"),
    ] {
        assert!(
            found.contains(&(krate.into(), table.into(), column.into())),
            "the scan misses {krate}.{table}.{column}"
        );
    }
    let invented = declared(
        "example",
        "CREATE TABLE IF NOT EXISTS widgets (id TEXT PRIMARY KEY, owner_id TEXT NOT NULL);\n\
         ALTER TABLE widgets ADD COLUMN IF NOT EXISTS approved_by TEXT;",
    );
    assert_eq!(
        invented,
        [
            ("example".into(), "widgets".into(), "owner_id".into()),
            ("example".into(), "widgets".into(), "approved_by".into()),
        ]
    );
    assert!(unclaimed(&invented.into_iter().collect()).len() == 2);
}

fn unclaimed(found: &BTreeSet<Column>) -> Vec<String> {
    found
        .iter()
        .filter(|(krate, table, column)| {
            !CLAIMS
                .iter()
                .any(|(k, t, c, _)| k == krate && t == table && c == column)
        })
        .map(|(krate, table, column)| format!("{krate}.{table}.{column}"))
        .collect()
}

#[test]
fn every_identity_column_is_claimed_by_a_category_or_declared_not_personal() {
    let missing = unclaimed(&scan());
    assert!(
        missing.is_empty(),
        "register these in a category's holding (or declare them not personal, with why):\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn every_claim_still_names_a_column_that_exists() {
    let found = scan();
    let stale: Vec<String> = CLAIMS
        .iter()
        .filter(|(k, t, c, _)| !found.contains(&(k.to_string(), t.to_string(), c.to_string())))
        .map(|(k, t, c, claim)| format!("{k}.{t}.{c} ({claim:?})"))
        .collect();
    assert!(
        stale.is_empty(),
        "claims for columns that are gone:\n  {}",
        stale.join("\n  ")
    );
}

#[test]
fn every_claim_names_a_category_or_says_why_it_is_not_personal() {
    for (krate, table, column, claim) in CLAIMS {
        match claim {
            By(category) => assert!(Category::ALL.contains(category)),
            NotPersonal(why) => assert!(
                !why.trim().is_empty(),
                "{krate}.{table}.{column} needs a reason"
            ),
        }
    }
}
