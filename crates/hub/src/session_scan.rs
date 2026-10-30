//! Filesystem helpers shared by the lesson and passive-memory backfill jobs.
//!
//! Both backfills walk an agent's `sessions/` directory, load each session's
//! `.meta.json` sidecar, parse its `.jsonl` transcript, and iterate
//! user→tool…→assistant turn boundaries. None of that is specific to which
//! extractor runs after the iteration, so the file-walking lives here.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tracing::{debug, warn};

use workforce_common::session::{MessageRole, SessionContextType, SessionMessage};
use workforce_core::types::AgentId;

/// Sidecar metadata for a session — the bits the backfills consume.
#[derive(serde::Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub channel: String,
    pub owner: String,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub context_type: Option<SessionContextType>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Return every session JSONL under `sessions_dir`, preferring the live file
/// over its `.archive.jsonl` sibling when both exist (the live file is the
/// hydrated, append-only authoritative copy). Logs and returns empty on a
/// missing/unreadable directory rather than erroring — backfill is best-
/// effort across many agents and one broken agent must not abort the run.
pub fn enumerate_session_files(sessions_dir: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(sessions_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %sessions_dir.display(), error = %e, "Cannot read sessions dir");
            return Vec::new();
        }
    };
    let mut live: Vec<PathBuf> = Vec::new();
    let mut archive: Vec<PathBuf> = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if name.ends_with(".archive.jsonl") {
            archive.push(path);
        } else if name.ends_with(".jsonl") && !name.ends_with(".meta.json") {
            live.push(path);
        }
    }
    let live_ids: std::collections::HashSet<String> = live
        .iter()
        .filter_map(|p| p.file_stem()?.to_str().map(|s| s.to_string()))
        .collect();
    archive.retain(|p| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".archive"))
            .map(|id| !live_ids.contains(id))
            .unwrap_or(false)
    });
    live.into_iter().chain(archive).collect()
}

/// Resolve the `.meta.json` sidecar path for a session file. Archive sessions
/// (`{uuid}.archive.jsonl`) share their meta with the live file
/// (`{uuid}.meta.json`); only the JSONL is renamed at archive time.
pub fn meta_path_for(session_path: &Path) -> PathBuf {
    let stem = session_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.strip_suffix(".archive").unwrap_or(s))
        .unwrap_or("");
    session_path.with_file_name(format!("{stem}.meta.json"))
}

/// Load a session's `.meta.json` sidecar.
pub fn load_session_meta(session_path: &Path) -> Result<SessionMeta> {
    let meta_path = meta_path_for(session_path);
    let raw = std::fs::read_to_string(&meta_path)
        .with_context(|| format!("reading session meta {}", meta_path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parsing session meta {}", meta_path.display()))
}

/// Parse a JSONL transcript, skipping lines that fail to deserialise (legacy
/// rows, partial writes). Returns an error only when the file itself is
/// unreadable — bad lines are logged at debug and elided so a single
/// corrupt row doesn't abort the whole session.
pub fn read_jsonl_messages(session_path: &Path) -> Result<Vec<SessionMessage>> {
    let raw = std::fs::read_to_string(session_path)
        .with_context(|| format!("reading session file {}", session_path.display()))?;
    let messages: Vec<SessionMessage> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<SessionMessage>(line) {
            Ok(m) => Some(m),
            Err(e) => {
                debug!(error = %e, "Skipping unparseable session line");
                None
            }
        })
        .collect();
    Ok(messages)
}

/// A complete user → tools → assistant turn boundary. Mirrors the live
/// extractor's `turn_messages` shape.
#[derive(Debug)]
pub struct Turn<'a> {
    pub user: &'a SessionMessage,
    pub assistant: &'a SessionMessage,
    pub between: &'a [SessionMessage],
}

pub struct TurnIter<'a> {
    messages: &'a [SessionMessage],
    cursor: usize,
}

impl<'a> Iterator for TurnIter<'a> {
    type Item = Turn<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let user_idx = self.messages[self.cursor..]
            .iter()
            .position(|m| m.role == MessageRole::User)?
            + self.cursor;
        let asst_idx = self.messages[user_idx + 1..]
            .iter()
            .position(|m| m.role == MessageRole::Assistant)
            .map(|i| i + user_idx + 1)?;
        self.cursor = asst_idx + 1;
        Some(Turn {
            user: &self.messages[user_idx],
            assistant: &self.messages[asst_idx],
            between: &self.messages[user_idx + 1..asst_idx],
        })
    }
}

pub fn iter_turns(messages: &[SessionMessage]) -> TurnIter<'_> {
    TurnIter {
        messages,
        cursor: 0,
    }
}

/// Resolve which agents to scan: the explicit list when non-empty, else every
/// agent directory under `agents_root`, sorted for deterministic ordering.
pub fn resolve_agent_ids(agents_root: &Path, explicit: &[AgentId]) -> Result<Vec<AgentId>> {
    if !explicit.is_empty() {
        return Ok(explicit.to_vec());
    }
    let entries = std::fs::read_dir(agents_root)?;
    let mut ids: Vec<AgentId> = entries
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .map(|n| AgentId::new(&n))
        .collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn msg(role: MessageRole, content: &str) -> SessionMessage {
        SessionMessage {
            id: uuid::Uuid::new_v4().to_string(),
            role,
            content: content.to_string(),
            tool_data: None,
            timestamp: Utc.with_ymd_and_hms(2026, 5, 17, 12, 0, 0).unwrap(),
            tokens: 0,
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        }
    }

    #[test]
    fn iter_yields_user_assistant_pair_with_tools_between() {
        let messages = vec![
            msg(MessageRole::User, "hi"),
            msg(MessageRole::Tool, ""),
            msg(MessageRole::Assistant, "hello"),
        ];
        let turns: Vec<_> = iter_turns(&messages).collect();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user.content, "hi");
        assert_eq!(turns[0].assistant.content, "hello");
        assert_eq!(turns[0].between.len(), 1);
    }

    #[test]
    fn iter_skips_user_without_assistant_follow_up() {
        let messages = vec![
            msg(MessageRole::User, "u1"),
            msg(MessageRole::Assistant, "a1"),
            msg(MessageRole::User, "u2-no-reply"),
        ];
        let turns: Vec<_> = iter_turns(&messages).collect();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user.content, "u1");
    }

    #[test]
    fn meta_path_strips_archive_suffix() {
        let p = Path::new("/tmp/abc-123.archive.jsonl");
        let meta = meta_path_for(p);
        assert_eq!(meta, Path::new("/tmp/abc-123.meta.json"));
    }

    #[test]
    fn meta_path_for_live_file() {
        let p = Path::new("/tmp/abc-123.jsonl");
        let meta = meta_path_for(p);
        assert_eq!(meta, Path::new("/tmp/abc-123.meta.json"));
    }

    #[test]
    fn resolve_agent_ids_returns_explicit_when_supplied() {
        let dir = tempfile::tempdir().unwrap();
        let explicit = vec![AgentId::new("allan"), AgentId::new("brian")];
        let resolved = resolve_agent_ids(dir.path(), &explicit).unwrap();
        assert_eq!(resolved, explicit);
    }

    #[test]
    fn resolve_agent_ids_enumerates_when_explicit_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("alpha")).unwrap();
        std::fs::create_dir(dir.path().join("beta")).unwrap();
        std::fs::write(dir.path().join("not-a-dir"), "x").unwrap();
        let resolved = resolve_agent_ids(dir.path(), &[]).unwrap();
        let names: Vec<&str> = resolved.iter().map(|a| a.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }
}
