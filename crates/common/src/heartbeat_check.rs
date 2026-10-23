//! Heartbeat check file system.
//!
//! Heartbeat checks are stored as individual markdown files with TOML frontmatter
//! under `{workspace}/heartbeats/`. Each file defines a periodic check the agent
//! should perform, with its own cron schedule, priority, and enabled state.
//!
//! File format:
//! ```markdown
//! +++
//! name = "Codebase Health Check"
//! enabled = true
//! schedule = "0 9,13,17 * * 1-5"
//! priority = 2
//! +++
//!
//! # Codebase Health Check
//! ...instructions for the agent...
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

fn default_priority() -> u32 {
    5
}

/// TOML frontmatter of a heartbeat check file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatCheckMeta {
    /// Human-readable name (e.g. "Codebase Health Check").
    pub name: String,
    /// Whether this check is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Cron expression for scheduling (e.g. "0 9,13,17,21 * * *").
    /// If omitted, inherits the global heartbeat interval.
    #[serde(default)]
    pub schedule: Option<String>,
    /// Priority: lower number = higher priority. Default 5.
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// Tags for grouping/filtering.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Timezone override for this check's cron expression.
    #[serde(default)]
    pub timezone: Option<String>,
    /// Cooldown in seconds: minimum time between two runs of this check.
    #[serde(default)]
    pub cooldown_secs: Option<u64>,
    /// Whether this check should take action instead of just monitoring.
    #[serde(default)]
    pub take_action: bool,
    /// The only tools the check's turn is offered, by name or namespace
    /// (e.g. `["github", "messaging.send"]`). Unset keeps the agent's full
    /// surface.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// The person who last created or edited the check through the hub.
    #[serde(default)]
    pub creator: Option<String>,
    /// The hub's signature over `creator` and the rest of the check: the
    /// check runs as its creator only while this verifies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator_sig: Option<String>,
}

/// A parsed heartbeat check file — frontmatter (TOML) + body (markdown).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatCheck {
    /// The file-stem ID (e.g. "codebase-review" from "codebase-review.md").
    pub id: String,
    /// Parsed frontmatter.
    pub meta: HeartbeatCheckMeta,
    /// Markdown body — the prompt/instructions for the agent.
    pub body: String,
}

/// Split `+++` TOML frontmatter from markdown body.
pub fn split_toml_frontmatter(content: &str) -> Result<(String, String)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("+++") {
        return Err(eyre!(
            "Heartbeat check must start with +++ TOML frontmatter"
        ));
    }
    let after_first = &trimmed[3..];
    let closing_pos = after_first
        .find("\n+++")
        .ok_or_else(|| eyre!("Missing closing +++ delimiter"))?;
    let frontmatter = after_first[..closing_pos].trim().to_string();
    let body = after_first[closing_pos + 4..].trim().to_string();
    Ok((frontmatter, body))
}

/// Parse a heartbeat check file from raw content + an ID.
pub fn parse_heartbeat_check(id: &str, content: &str) -> Result<HeartbeatCheck> {
    let (frontmatter, body) = split_toml_frontmatter(content)?;
    let meta: HeartbeatCheckMeta = toml::from_str(&frontmatter).map_err(|e| {
        eyre!(
            "Failed to parse heartbeat check '{}' frontmatter: {}",
            id,
            e
        )
    })?;
    Ok(HeartbeatCheck {
        id: id.to_string(),
        meta,
        body,
    })
}

/// Serialize a heartbeat check back to the `+++` frontmatter format.
pub fn serialize_heartbeat_check(check: &HeartbeatCheck) -> Result<String> {
    let frontmatter = toml::to_string_pretty(&check.meta)
        .map_err(|e| eyre!("Failed to serialize heartbeat check '{}': {}", check.id, e))?;
    Ok(format!(
        "+++\n{}\n+++\n\n{}\n",
        frontmatter.trim(),
        check.body
    ))
}

/// Return the heartbeats directory for a workspace.
pub fn heartbeats_dir(workspace: &Path) -> PathBuf {
    workspace.join("heartbeats")
}

/// List all heartbeat checks from disk, sorted by priority then ID.
pub fn list_heartbeat_checks(workspace: &Path) -> Result<Vec<HeartbeatCheck>> {
    let dir = heartbeats_dir(workspace);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut checks = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let content = fs::read_to_string(&path)?;
            match parse_heartbeat_check(&id, &content) {
                Ok(check) => checks.push(check),
                Err(e) => {
                    tracing::warn!("Skipping invalid heartbeat file {}: {}", path.display(), e);
                }
            }
        }
    }

    checks.sort_by(|a, b| a.meta.priority.cmp(&b.meta.priority).then(a.id.cmp(&b.id)));
    Ok(checks)
}

/// Load a single heartbeat check by ID.
pub fn load_heartbeat_check(workspace: &Path, id: &str) -> Result<HeartbeatCheck> {
    let path = heartbeats_dir(workspace).join(format!("{id}.md"));
    if !path.exists() {
        return Err(eyre!("Heartbeat check '{}' not found", id));
    }
    let content = fs::read_to_string(&path)?;
    parse_heartbeat_check(id, &content)
}

/// Save a heartbeat check to disk, creating the heartbeats directory if needed.
pub fn save_heartbeat_check(workspace: &Path, check: &HeartbeatCheck) -> Result<()> {
    let dir = heartbeats_dir(workspace);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", check.id));
    let content = serialize_heartbeat_check(check)?;
    fs::write(&path, content)?;
    Ok(())
}

/// Delete a heartbeat check from disk.
pub fn delete_heartbeat_check(workspace: &Path, id: &str) -> Result<()> {
    let path = heartbeats_dir(workspace).join(format!("{id}.md"));
    if !path.exists() {
        return Err(eyre!("Heartbeat check '{}' not found", id));
    }
    fs::remove_file(&path)?;
    Ok(())
}

/// Return built-in heartbeat check templates.
pub fn builtin_heartbeat_checks() -> Vec<HeartbeatCheck> {
    vec![
        HeartbeatCheck {
            id: "linear-tickets".to_string(),
            meta: HeartbeatCheckMeta {
                name: "Linear Ticket Monitoring".to_string(),
                enabled: false,
                schedule: Some("0 9,13,17 * * 1-5".to_string()),
                priority: 2,
                tags: vec!["project-management".into()],
                timezone: None,
                cooldown_secs: None,
                take_action: false,
                tools: None,
                creator: None,
                creator_sig: None,
            },
            body: "# Linear Ticket Monitoring\n\n\
                ## What to check:\n\
                - Review tickets assigned to you or the team\n\
                - Check for blockers and urgent issues\n\
                - Monitor ticket age and staleness\n\
                - Alert on priority changes or newly critical items\n\n\
                ## Output:\n\
                - If issues found: describe findings clearly\n\
                - If nothing needs attention: reply HEARTBEAT_OK"
                .to_string(),
        },
        HeartbeatCheck {
            id: "github-prs".to_string(),
            meta: HeartbeatCheckMeta {
                name: "GitHub PR Monitoring".to_string(),
                enabled: false,
                schedule: Some("0 10,14,18 * * 1-5".to_string()),
                priority: 2,
                tags: vec!["code-review".into()],
                timezone: None,
                cooldown_secs: None,
                take_action: false,
                tools: None,
                creator: None,
                creator_sig: None,
            },
            body: "# GitHub PR Monitoring\n\n\
                ## What to check:\n\
                - Review PRs awaiting review\n\
                - Monitor CI/CD status on open PRs\n\
                - Track PR age and pending review requests\n\
                - Alert on merge conflicts\n\n\
                ## Output:\n\
                - If issues found: describe findings clearly\n\
                - If nothing needs attention: reply HEARTBEAT_OK"
                .to_string(),
        },
        HeartbeatCheck {
            id: "slack-mentions".to_string(),
            meta: HeartbeatCheckMeta {
                name: "Slack Mention Tracking".to_string(),
                enabled: false,
                schedule: Some("0 */2 * * *".to_string()),
                priority: 1,
                tags: vec!["communication".into()],
                timezone: None,
                cooldown_secs: None,
                take_action: false,
                tools: None,
                creator: None,
                creator_sig: None,
            },
            body: "# Slack Mention Tracking\n\n\
                ## What to check:\n\
                - Monitor @mentions across channels\n\
                - Track DM messages that need responses\n\
                - Check for unanswered questions\n\
                - Alert on high-priority mentions\n\n\
                ## Output:\n\
                - If issues found: describe findings clearly\n\
                - If nothing needs attention: reply HEARTBEAT_OK"
                .to_string(),
        },
        HeartbeatCheck {
            id: "session-tasks".to_string(),
            meta: HeartbeatCheckMeta {
                name: "Session Task Management".to_string(),
                enabled: false,
                schedule: Some("0 9,17 * * *".to_string()),
                priority: 3,
                tags: vec!["task-management".into()],
                timezone: None,
                cooldown_secs: None,
                take_action: false,
                tools: None,
                creator: None,
                creator_sig: None,
            },
            body: "# Session Task Management\n\n\
                ## What to check:\n\
                - Use notes.list_all_tasks to check for incomplete work\n\
                - Alert on stalled tasks across sessions\n\
                - Track task age and blockers\n\
                - Suggest task resumption when appropriate\n\n\
                ## Output:\n\
                - If issues found: describe findings clearly\n\
                - If nothing needs attention: reply HEARTBEAT_OK"
                .to_string(),
        },
        HeartbeatCheck {
            id: "maintenance".to_string(),
            meta: HeartbeatCheckMeta {
                name: "Proactive Maintenance".to_string(),
                enabled: false,
                schedule: Some("0 6 * * *".to_string()),
                priority: 5,
                tags: vec!["maintenance".into()],
                timezone: None,
                cooldown_secs: None,
                take_action: false,
                tools: None,
                creator: None,
                creator_sig: None,
            },
            body: "# Proactive Maintenance\n\n\
                ## What to do:\n\
                - Read and organize memory files\n\
                - Clean up completed session tasks\n\
                - Update documentation if needed\n\
                - Check for stale data or outdated information\n\n\
                ## Output:\n\
                - Describe what maintenance was performed\n\
                - If nothing needs attention: reply HEARTBEAT_OK"
                .to_string(),
        },
    ]
}

/// Write built-in heartbeat checks that don't already exist on disk.
/// Returns the IDs of checks that were created.
pub fn init_builtin_checks(workspace: &Path) -> Result<Vec<String>> {
    let dir = heartbeats_dir(workspace);
    fs::create_dir_all(&dir)?;

    let mut created = Vec::new();
    for check in builtin_heartbeat_checks() {
        let path = dir.join(format!("{}.md", check.id));
        if !path.exists() {
            save_heartbeat_check(workspace, &check)?;
            created.push(check.id);
        }
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_frontmatter_basic() {
        let content = "+++\nname = \"Test\"\n+++\n\n# Body here";
        let (fm, body) = split_toml_frontmatter(content).unwrap();
        assert_eq!(fm, "name = \"Test\"");
        assert_eq!(body, "# Body here");
    }

    #[test]
    fn split_frontmatter_multiline() {
        let content = "+++\nname = \"Test\"\nenabled = true\npriority = 1\n+++\n\nBody text";
        let (fm, body) = split_toml_frontmatter(content).unwrap();
        assert!(fm.contains("name = \"Test\""));
        assert!(fm.contains("priority = 1"));
        assert_eq!(body, "Body text");
    }

    #[test]
    fn split_frontmatter_missing_opening() {
        let content = "# No frontmatter";
        assert!(split_toml_frontmatter(content).is_err());
    }

    #[test]
    fn split_frontmatter_missing_closing() {
        let content = "+++\nname = \"Test\"\nno closing";
        assert!(split_toml_frontmatter(content).is_err());
    }

    #[test]
    fn parse_check_full() {
        let content = "+++\n\
            name = \"Health Check\"\n\
            enabled = true\n\
            schedule = \"0 9 * * *\"\n\
            priority = 1\n\
            tags = [\"monitoring\"]\n\
            timezone = \"Europe/London\"\n\
            cooldown_secs = 3600\n\
            +++\n\n\
            # Health Check\n\
            Check all systems.";
        let check = parse_heartbeat_check("health", content).unwrap();
        assert_eq!(check.id, "health");
        assert_eq!(check.meta.name, "Health Check");
        assert!(check.meta.enabled);
        assert_eq!(check.meta.schedule, Some("0 9 * * *".to_string()));
        assert_eq!(check.meta.priority, 1);
        assert_eq!(check.meta.tags, vec!["monitoring"]);
        assert_eq!(check.meta.timezone, Some("Europe/London".to_string()));
        assert_eq!(check.meta.cooldown_secs, Some(3600));
        assert!(check.body.contains("Check all systems"));
    }

    #[test]
    fn parse_check_minimal() {
        let content = "+++\nname = \"Minimal\"\n+++\n\nDo stuff.";
        let check = parse_heartbeat_check("min", content).unwrap();
        assert_eq!(check.meta.name, "Minimal");
        assert!(check.meta.enabled); // default true
        assert_eq!(check.meta.priority, 5); // default
        assert!(check.meta.schedule.is_none());
        assert!(check.meta.tags.is_empty());
    }

    #[test]
    fn heartbeat_check_tools_frontmatter_parses_and_defaults_none() {
        let scoped = parse_heartbeat_check(
            "inbox",
            "+++\nname = \"Inbox\"\ntools = [\"github\", \"messaging.send\"]\n+++\n\nTriage.",
        )
        .unwrap();
        assert_eq!(
            scoped.meta.tools.as_deref(),
            Some(&["github".to_string(), "messaging.send".to_string()][..])
        );
        let reparsed =
            parse_heartbeat_check("inbox", &serialize_heartbeat_check(&scoped).unwrap()).unwrap();
        assert_eq!(reparsed.meta.tools, scoped.meta.tools);

        let unscoped =
            parse_heartbeat_check("min", "+++\nname = \"Minimal\"\n+++\n\nDo stuff.").unwrap();
        assert_eq!(unscoped.meta.tools, None);
    }

    #[test]
    fn serialize_roundtrip() {
        let check = HeartbeatCheck {
            id: "test".to_string(),
            meta: HeartbeatCheckMeta {
                name: "Test Check".to_string(),
                enabled: true,
                schedule: Some("0 9 * * *".to_string()),
                priority: 2,
                tags: vec!["tag1".into()],
                timezone: None,
                cooldown_secs: None,
                take_action: false,
                tools: None,
                creator: None,
                creator_sig: None,
            },
            body: "# Test\nDo the thing.".to_string(),
        };
        let serialized = serialize_heartbeat_check(&check).unwrap();
        assert!(serialized.starts_with("+++\n"));
        assert!(serialized.contains("name = \"Test Check\""));
        assert!(serialized.contains("# Test\nDo the thing."));

        // Can re-parse
        let reparsed = parse_heartbeat_check("test", &serialized).unwrap();
        assert_eq!(reparsed.meta.name, "Test Check");
        assert_eq!(reparsed.meta.schedule, Some("0 9 * * *".to_string()));
        assert_eq!(reparsed.body, "# Test\nDo the thing.");
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let check = HeartbeatCheck {
            id: "test-check".to_string(),
            meta: HeartbeatCheckMeta {
                name: "Test Check".to_string(),
                enabled: true,
                schedule: Some("0 */2 * * *".to_string()),
                priority: 3,
                tags: vec![],
                timezone: None,
                take_action: false,
                cooldown_secs: None,
                tools: None,
                creator: None,
                creator_sig: None,
            },
            body: "# Test\nCheck things.".to_string(),
        };

        save_heartbeat_check(dir.path(), &check).unwrap();
        let loaded = load_heartbeat_check(dir.path(), "test-check").unwrap();

        assert_eq!(loaded.id, "test-check");
        assert_eq!(loaded.meta.name, "Test Check");
        assert_eq!(loaded.meta.schedule, Some("0 */2 * * *".to_string()));
        assert_eq!(loaded.meta.priority, 3);
    }

    #[test]
    fn list_checks_sorted_by_priority() {
        let dir = tempfile::tempdir().unwrap();

        let low = HeartbeatCheck {
            id: "low-priority".to_string(),
            meta: HeartbeatCheckMeta {
                name: "Low".to_string(),
                enabled: true,
                schedule: None,
                priority: 10,
                tags: vec![],
                take_action: false,
                timezone: None,
                cooldown_secs: None,
                tools: None,
                creator: None,
                creator_sig: None,
            },
            body: "Low priority check.".to_string(),
        };
        let high = HeartbeatCheck {
            id: "high-priority".to_string(),
            meta: HeartbeatCheckMeta {
                name: "High".to_string(),
                enabled: true,
                schedule: None,
                priority: 1,
                take_action: false,
                tags: vec![],
                timezone: None,
                cooldown_secs: None,
                tools: None,
                creator: None,
                creator_sig: None,
            },
            body: "High priority check.".to_string(),
        };

        save_heartbeat_check(dir.path(), &low).unwrap();
        save_heartbeat_check(dir.path(), &high).unwrap();

        let checks = list_heartbeat_checks(dir.path()).unwrap();
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].id, "high-priority");
        assert_eq!(checks[1].id, "low-priority");
    }

    #[test]
    fn delete_check_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let check = HeartbeatCheck {
            id: "doomed".to_string(),
            meta: HeartbeatCheckMeta {
                name: "Doomed".to_string(),
                enabled: true,
                schedule: None,
                take_action: false,
                priority: 5,
                tags: vec![],
                timezone: None,
                cooldown_secs: None,
                tools: None,
                creator: None,
                creator_sig: None,
            },
            body: "bye".to_string(),
        };
        save_heartbeat_check(dir.path(), &check).unwrap();
        assert!(load_heartbeat_check(dir.path(), "doomed").is_ok());

        delete_heartbeat_check(dir.path(), "doomed").unwrap();
        assert!(load_heartbeat_check(dir.path(), "doomed").is_err());
    }

    #[test]
    fn builtin_checks_count() {
        let checks = builtin_heartbeat_checks();
        assert_eq!(checks.len(), 5);
        let ids: Vec<&str> = checks.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"linear-tickets"));
        assert!(ids.contains(&"github-prs"));
        assert!(ids.contains(&"slack-mentions"));
        assert!(ids.contains(&"session-tasks"));
        assert!(ids.contains(&"maintenance"));
    }

    #[test]
    fn init_builtin_checks_idempotent() {
        let dir = tempfile::tempdir().unwrap();

        let created1 = init_builtin_checks(dir.path()).unwrap();
        assert_eq!(created1.len(), 5);

        let created2 = init_builtin_checks(dir.path()).unwrap();
        assert_eq!(created2.len(), 0, "Second init should skip existing checks");

        let all = list_heartbeat_checks(dir.path()).unwrap();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn list_checks_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let checks = list_heartbeat_checks(dir.path()).unwrap();
        assert!(checks.is_empty());
    }

    #[test]
    fn load_nonexistent_check_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_heartbeat_check(dir.path(), "nope").is_err());
    }

    #[test]
    fn delete_nonexistent_check_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(delete_heartbeat_check(dir.path(), "nope").is_err());
    }
}
