//! `sessions.search` — full-text search over the calling user's session
//! transcripts, and `sessions.read` — one hit's message in full. The agent's
//! fallback when `memory.recall` returns nothing relevant: a memory may have
//! been silently dropped during a regression window, but the raw transcript
//! is still on disk.
//!
//! Hard isolation rule (security): this tool MUST NEVER surface a session
//! whose owner is not the current `CURRENT_USER_ID`. The store does not
//! enforce ownership, so the tool checks every session it reads here.
//! When `CURRENT_USER_ID` is unscoped the tool refuses to run, mirroring
//! `MemoryTool`'s DW-518 / DW-520 behaviour.
//!
//! This tool is read-only and per-user. It does not require the agent's
//! session-write permissions.
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::session::{SessionSearchResult, SessionStore, TranscriptEntry};

/// Hard cap on result count regardless of caller-supplied `limit`. Keeps a
/// noisy query from blowing up the agent's context window.
const MAX_LIMIT: usize = 25;

/// Default result count when the caller doesn't specify.
const DEFAULT_LIMIT: usize = 10;

/// Over-fetch factor: the store search has no per-user filter, so we pull
/// more than the caller asked for and drop sessions the caller doesn't own.
/// Capped to a sane absolute (no point fetching 10k rows to filter to 25).
const OVERFETCH_FACTOR: usize = 4;
const OVERFETCH_MAX: usize = 200;

pub struct SessionsTool {
    sessions: Arc<dyn SessionStore>,
}

impl SessionsTool {
    pub fn new(sessions: Arc<dyn SessionStore>) -> Self {
        Self { sessions }
    }
}

#[derive(Deserialize)]
struct SessionsInput {
    action: String,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    message_id: Option<String>,
}

/// Whether the caller may read a session.
enum Scope {
    Visible,
    /// An eval's turns are never a source: one question's answer must not be
    /// found by the next, nor by the person's own chats.
    Eval,
    OtherUser,
    Missing,
}

#[async_trait]
impl Tool for SessionsTool {
    fn name(&self) -> &str {
        "sessions"
    }

    fn description(&self) -> &str {
        "Search your own past session transcripts. Use this as a fallback \
         when memory.recall returns nothing relevant — raw conversation \
         text may still answer the question. Search covers the full text of \
         the messages you sent through messaging and email tools as well as \
         the conversation itself. `search` returns matching excerpts scoped \
         strictly to your own sessions; `read` returns one hit's message in \
         full."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["search", "read"],
                    "description": "`search` finds excerpts; `read` returns one message from a \
                        search hit in full."
                },
                "query": {
                    "type": "string",
                    "description": "For `search`: substring to search for across your past \
                        transcripts (e.g. a hotel name, ticket id, or person). Case-insensitive."
                },
                "session_id": {
                    "type": "string",
                    "description": "For `read`: the `session_id` of a search hit."
                },
                "message_id": {
                    "type": "string",
                    "description": "For `read`: the `message_id` of a search hit."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 25,
                    "description": "Max excerpts to return (default 10, cap 25)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let parsed: SessionsInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("invalid input: {e}"),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        match parsed.action.as_str() {
            "search" => self.handle_search(parsed, start).await,
            "read" => Ok(self.handle_read(parsed, start)),
            other => Ok(ToolResult::failure(
                format!("Unknown action '{other}'. Use: search, read"),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

impl SessionsTool {
    async fn handle_search(&self, input: SessionsInput, start: Instant) -> Result<ToolResult> {
        let identity_id = match scoped_identity("search", start) {
            Ok(id) => id,
            Err(refusal) => return Ok(refusal),
        };

        let query = match input.query {
            Some(q) if !q.trim().is_empty() => q.trim().to_string(),
            _ => {
                return Ok(ToolResult::failure(
                    "Missing 'query' — what should be searched?",
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let requested = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let fetch_n = requested
            .saturating_mul(OVERFETCH_FACTOR)
            .clamp(requested, OVERFETCH_MAX);

        let raw = match self.sessions.search_sessions(&query, fetch_n) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("session search failed: {e}"),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        // Visit the entire fetched window before slicing so the suppression
        // counters reflect the real cross-user volume over `fetch_n` rather
        // than under-reporting whatever sat past the first `requested`
        // matches. The counters drive the audit log below.
        let mut suppressed_other_user = 0usize;
        let mut suppressed_missing = 0usize;
        let owned: Vec<SessionSearchResult> = raw
            .into_iter()
            .filter(|r| match self.scope(&r.session_id, &identity_id) {
                Scope::Visible => true,
                Scope::Eval => false,
                Scope::OtherUser => {
                    suppressed_other_user += 1;
                    false
                }
                Scope::Missing => {
                    suppressed_missing += 1;
                    false
                }
            })
            .collect();
        let visible: Vec<SessionSearchResult> = owned.into_iter().take(requested).collect();

        if suppressed_other_user > 0 {
            tracing::info!(
                identity = %identity_id,
                suppressed = suppressed_other_user,
                "sessions.search filtered out matches from sessions owned by other users"
            );
        }

        let results_json: Vec<serde_json::Value> = visible
            .iter()
            .map(|r| {
                serde_json::json!({
                    "session_id": r.session_id,
                    "label": r.label,
                    "message_id": r.message_id,
                    "role": r.role,
                    "excerpt": r.excerpt,
                    "timestamp": r.timestamp.to_rfc3339(),
                })
            })
            .collect();

        Ok(ToolResult::success(
            serde_json::json!({
                "action": "search",
                "query": query,
                "count": results_json.len(),
                "results": results_json,
                "suppressed_other_user": suppressed_other_user,
                "suppressed_missing": suppressed_missing,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }

    fn handle_read(&self, input: SessionsInput, start: Instant) -> ToolResult {
        let identity_id = match scoped_identity("read", start) {
            Ok(id) => id,
            Err(refusal) => return refusal,
        };
        let ids = input
            .session_id
            .zip(input.message_id)
            .map(|(s, m)| (s.trim().to_string(), m.trim().to_string()))
            .filter(|(s, m)| !s.is_empty() && !m.is_empty());
        let Some((session_id, message_id)) = ids else {
            return ToolResult::failure(
                "Missing 'session_id' or 'message_id' — pass both from a search hit",
                start.elapsed().as_millis() as u64,
            );
        };
        let entry = match self.scope(&session_id, &identity_id) {
            Scope::Visible => self.sessions.read_archive_message(&session_id, &message_id),
            Scope::Eval | Scope::OtherUser | Scope::Missing => Ok(None),
        };
        let elapsed = start.elapsed().as_millis() as u64;
        match entry {
            Ok(Some(entry)) => ToolResult::success(read_result(&entry), elapsed),
            Ok(None) => ToolResult::failure(
                format!("No message '{message_id}' in session '{session_id}'"),
                elapsed,
            ),
            Err(e) => ToolResult::failure(format!("session read failed: {e}"), elapsed),
        }
    }

    /// The one scope rule both actions apply: the caller's own sessions,
    /// never an eval's.
    fn scope(&self, session_id: &str, identity_id: &str) -> Scope {
        match self.sessions.get(session_id) {
            Ok(Some(s)) if crate::turn_extraction::is_eval_channel(&s.channel) => Scope::Eval,
            Ok(Some(s)) if s.owner == identity_id => Scope::Visible,
            Ok(Some(_)) => Scope::OtherUser,
            _ => Scope::Missing,
        }
    }
}

/// The caller's identity, or the refusal to run unscoped. Mirrors the memory
/// tool's DW-518 / DW-520 guard so a runaway code path that forgets to wrap
/// with `CURRENT_USER_ID.scope(...)` cannot leak another user's transcripts.
fn scoped_identity(action: &str, start: Instant) -> Result<String, ToolResult> {
    crate::user_config::current_identity_id().ok_or_else(|| {
        ToolResult::failure(
            format!(
                "sessions.{action} requires a user identity in scope; refusing to run unscoped"
            ),
            start.elapsed().as_millis() as u64,
        )
    })
}

fn read_result(entry: &TranscriptEntry) -> serde_json::Value {
    serde_json::json!({
        "action": "read",
        "session_id": entry.session_id,
        "message_id": entry.id,
        "role": entry.role,
        "content": entry.content,
        "timestamp": entry.timestamp.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_common::session::{InMemorySessionStore, MessageRole, Session, SessionMessage};

    fn seed_session(store: &InMemorySessionStore, owner: &str, text: &str) -> String {
        let mut s = Session::new("slack:C1:T1", owner, "allan");
        let msg = SessionMessage {
            id: format!("m-{owner}-{}", text.len()),
            role: MessageRole::User,
            content: text.to_string(),
            tool_data: None,
            timestamp: chrono::Utc::now(),
            tokens: 0,
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
        };
        s.messages.push(msg);
        let id = s.id.clone();
        store.save(&s).expect("save");
        id
    }

    #[tokio::test]
    async fn search_returns_only_callers_own_sessions() {
        let store = Arc::new(InMemorySessionStore::new());
        let _mine = seed_session(store.as_ref(), "user-A", "Uganda hotel Kampala");
        let _other = seed_session(store.as_ref(), "user-B", "Uganda hotel Kampala");

        let tool = SessionsTool::new(store.clone());
        let out = CURRENT_USER_ID
            .scope("user-A".to_string(), async {
                tool.execute(serde_json::json!({
                    "action": "search",
                    "query": "Uganda hotel",
                }))
                .await
                .expect("execute")
            })
            .await;

        assert!(out.success, "expected success, got {:?}", out);
        let count = out.output["count"].as_u64().unwrap_or(0);
        assert_eq!(count, 1, "expected exactly the caller's own session");
        let suppressed = out.output["suppressed_other_user"].as_u64().unwrap_or(0);
        assert_eq!(
            suppressed, 1,
            "expected one cross-user match to be filtered out"
        );
    }

    #[tokio::test]
    async fn search_never_finds_an_evals_turn() {
        let store = Arc::new(InMemorySessionStore::new());
        let asked = seed_session(store.as_ref(), "user-A", "Uganda hotel Kampala");
        let mut eval = store.get(&asked).unwrap().unwrap();
        eval.channel = "eval:t1".into();
        store.save(&eval).unwrap();
        let chatted = seed_session(store.as_ref(), "user-A", "Uganda hotel in Kampala");

        let tool = SessionsTool::new(store.clone());
        let out = CURRENT_USER_ID
            .scope("user-A".to_string(), async {
                tool.execute(serde_json::json!({"action": "search", "query": "Uganda hotel"}))
                    .await
                    .expect("execute")
            })
            .await;
        let found: Vec<&str> = out.output["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["session_id"].as_str())
            .collect();
        assert_eq!(found, [chatted.as_str()]);
    }

    #[tokio::test]
    async fn search_refuses_without_scoped_identity() {
        let store = Arc::new(InMemorySessionStore::new());
        let _ = seed_session(store.as_ref(), "user-A", "Uganda");
        let tool = SessionsTool::new(store.clone());

        let out = tool
            .execute(serde_json::json!({
                "action": "search",
                "query": "Uganda",
            }))
            .await
            .expect("execute");
        assert!(!out.success);
        let msg = out.error.as_deref().unwrap_or_default();
        assert!(
            msg.contains("unscoped") || msg.contains("identity"),
            "expected an unscoped-identity failure, got {msg}"
        );
    }

    #[tokio::test]
    async fn empty_query_is_rejected() {
        let store = Arc::new(InMemorySessionStore::new());
        let tool = SessionsTool::new(store);
        let out = CURRENT_USER_ID
            .scope("user-A".to_string(), async {
                tool.execute(serde_json::json!({
                    "action": "search",
                    "query": "   ",
                }))
                .await
                .expect("execute")
            })
            .await;
        assert!(!out.success);
    }

    fn seed_delivery(store: &InMemorySessionStore, owner: &str, text: &str) -> (String, String) {
        let mut s = Session::new("slack:D1", owner, "allan");
        s.add_user_message("send Sam the lyrics", None);
        let message_id = s.add_tool_message(
            "messaging.send",
            &serde_json::json!({ "channel": "D1", "text": text }),
            &serde_json::json!({ "ok": true }),
            true,
            12,
            None,
        );
        store.save(&s).expect("save");
        (s.id, message_id)
    }

    async fn run_as(tool: &SessionsTool, user: &str, input: serde_json::Value) -> ToolResult {
        CURRENT_USER_ID
            .scope(user.to_string(), async {
                tool.execute(input).await.expect("execute")
            })
            .await
    }

    fn read(session_id: &str, message_id: &str) -> serde_json::Value {
        serde_json::json!({
            "action": "read",
            "session_id": session_id,
            "message_id": message_id,
        })
    }

    #[tokio::test]
    async fn a_long_sent_message_is_found_and_read_back_in_full() {
        let store = Arc::new(InMemorySessionStore::new());
        let lyrics = format!("{} marmalade skies", "la ".repeat(3000));
        seed_delivery(store.as_ref(), "user-A", &lyrics);
        let tool = SessionsTool::new(store.clone());

        let found = run_as(
            &tool,
            "user-A",
            serde_json::json!({"action": "search", "query": "marmalade"}),
        )
        .await;
        let hit = &found.output["results"][0];
        assert_eq!(hit["role"], "tool");

        let out = run_as(
            &tool,
            "user-A",
            read(
                hit["session_id"].as_str().unwrap(),
                hit["message_id"].as_str().unwrap(),
            ),
        )
        .await;
        assert!(out.success, "{out:?}");
        assert_eq!(out.output["role"], "tool");
        assert_eq!(
            out.output["content"],
            format!(
                "Tool 'messaging.send' completed in 12ms\n\n{}",
                lyrics.trim()
            )
        );
    }

    #[tokio::test]
    async fn read_finds_nothing_outside_the_callers_own_sessions() {
        let store = Arc::new(InMemorySessionStore::new());
        let (mine, mine_msg) = seed_delivery(store.as_ref(), "user-A", "hello");
        let (theirs, theirs_msg) = seed_delivery(store.as_ref(), "user-B", "hello");
        let (eval, eval_msg) = seed_delivery(store.as_ref(), "user-A", "hello");
        let mut eval_session = store.get(&eval).unwrap().unwrap();
        eval_session.channel = "eval:t1".into();
        store.save(&eval_session).unwrap();
        let tool = SessionsTool::new(store.clone());

        for (session_id, message_id) in [
            (theirs.as_str(), theirs_msg.as_str()),
            (eval.as_str(), eval_msg.as_str()),
            (mine.as_str(), "no-such-message"),
            ("no-such-session", mine_msg.as_str()),
        ] {
            let out = run_as(&tool, "user-A", read(session_id, message_id)).await;
            assert!(
                !out.success,
                "{session_id}/{message_id} must not be readable"
            );
            assert_eq!(
                out.error.as_deref(),
                Some(format!("No message '{message_id}' in session '{session_id}'").as_str())
            );
        }
        assert!(
            run_as(&tool, "user-A", read(&mine, &mine_msg))
                .await
                .success
        );
    }

    #[tokio::test]
    async fn read_refuses_without_scoped_identity_or_ids() {
        let store = Arc::new(InMemorySessionStore::new());
        let (session_id, message_id) = seed_delivery(store.as_ref(), "user-A", "hello");
        let tool = SessionsTool::new(store.clone());

        let unscoped = tool
            .execute(read(&session_id, &message_id))
            .await
            .expect("execute");
        assert!(!unscoped.success);
        assert!(unscoped
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("unscoped"));

        let missing = run_as(
            &tool,
            "user-A",
            serde_json::json!({"action": "read", "session_id": session_id}),
        )
        .await;
        assert!(!missing.success);
    }
}
