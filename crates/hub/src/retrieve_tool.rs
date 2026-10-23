//! `tool.retrieve` — recover the full original of a tool output that an earlier
//! compaction pass truncated or evicted from the context window.
//!
//! Phase 1 of tool-output compression.
//! When `Session::prune_tool_outputs` / `evict_tool_outputs` shrink a large tool
//! result, they first stash the pristine original under `tool_data.output_full`
//! and embed a `retrieve_handle` in the marker the model sees. This tool reads
//! that stash back from the **current session** so the agent can recover specific
//! data (a row, a path, a field) instead of re-running the tool.
//!
//! It also recovers blobs the auto-spill backstop staged out of context: an
//! `art_…` handle resolves from the artifact store (session/user-scoped) rather
//! than the session stash, sharing the same query-filter and egress scrubbing.
//!
//! Scope: current-session, cross-invocation retrieval. Outputs persist only after
//! a run completes, so within-run mid-round retrieval is out of scope here (it
//! belongs to the Phase 2 in-memory ingest cache).
//!
//! Security: `output_full` holds the UNtokenised original and is read by NOTHING
//! else — no model-facing serialization touches it. This tool re-runs the same
//! per-session credential tokenisation as the ingress path before returning it,
//! so a secret the ingress layer would have caught cannot re-enter the prompt.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use workforce_agent::artifacts::{
    global_store, read_session_scoped, value_to_text, ArtifactBody, ArtifactId,
};
use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_SESSION_ID};
use workforce_agent::security::external_content::tokenize_for_egress;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::session::{MessageRole, SessionStore};

/// Default cap on returned content so a retrieve doesn't re-bloat the very
/// context the original was compacted out of. The agent is nudged to pass a
/// `query` to narrow further.
const DEFAULT_MAX_CHARS: usize = 6000;
/// Absolute ceiling regardless of caller-supplied `max_chars`.
const HARD_MAX_CHARS: usize = 24000;

pub struct RetrieveTool {
    sessions: Arc<dyn SessionStore>,
}

impl RetrieveTool {
    pub fn new(sessions: Arc<dyn SessionStore>) -> Self {
        Self { sessions }
    }
}

#[derive(Deserialize)]
struct RetrieveInput {
    /// Exact handle from a marker, e.g. `h:ab12cd34ef`.
    #[serde(default)]
    handle: Option<String>,
    /// Alternative selector: the tool name whose latest stashed/intact output to
    /// fetch (e.g. `code.search`). Used when no handle is in the transcript.
    #[serde(default)]
    tool: Option<String>,
    /// Case-insensitive substring; only matching lines are returned.
    #[serde(default)]
    query: Option<String>,
    /// Cap on returned characters (clamped to [`HARD_MAX_CHARS`]).
    #[serde(default)]
    max_chars: Option<usize>,
}

/// The retrievable original of a tool message: the stashed `output_full`, or the
/// live `output` when it hasn't been evicted to a marker yet. `None` when the
/// original is gone (evicted before Phase 1, or skipped by the stash byte-cap).
fn original_of(tool_data: &serde_json::Value) -> Option<(String, String)> {
    let tool_name = tool_data
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let as_str = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if let Some(full) = tool_data.get("output_full") {
        return Some((tool_name, as_str(full)));
    }
    // No stash: the live output is only usable if it isn't itself a marker.
    let out = tool_data.get("output")?;
    let s = as_str(out);
    if s.starts_with("[Output evicted") || s.contains("[output truncated") {
        return None;
    }
    Some((tool_name, s))
}

#[async_trait]
impl Tool for RetrieveTool {
    fn name(&self) -> &str {
        // Shared with the spill decision, which stages a blob only when this
        // tool is registered. A divergence here would silently strand every
        // blob it stages, so the two read the same constant.
        workforce_agent::spill::RETRIEVE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Recover the full original of a tool output that was truncated or evicted \
         from the conversation to save context. Pass the `handle` from a marker \
         like `tool.retrieve(handle=\"h:ab12cd34ef\")`, or a `tool` name to fetch \
         its latest output. Add a `query` to return only matching lines. Prefer a \
         narrow query over fetching everything — the output was compacted for a reason."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": { "type": "string", "description": "Exact handle from a marker, e.g. `h:ab12cd34ef`." },
                "tool": { "type": "string", "description": "Alternative to handle: tool name whose latest output to fetch (e.g. `code.search`)." },
                "query": { "type": "string", "description": "Case-insensitive substring; only matching lines are returned." },
                "max_chars": { "type": "integer", "description": "Cap on returned characters (default 6000)." }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        let start = Instant::now();
        let duration = || start.elapsed().as_millis() as u64;

        let parsed: RetrieveInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("invalid input: {e}"),
                    duration(),
                ))
            }
        };
        if parsed.handle.is_none() && parsed.tool.is_none() {
            return Ok(ToolResult::failure(
                "provide either `handle` (from a marker) or `tool` (a tool name)",
                duration(),
            ));
        }

        // An `art_…` handle is a blob the spill backstop staged out of context;
        // resolve it from the artifact store (session/user-scoped) instead of
        // the session stash.
        if let Some(id) = parsed
            .handle
            .as_deref()
            .and_then(|h| ArtifactId::parse(h).ok())
        {
            return Ok(retrieve_artifact(
                &id,
                parsed.query.as_deref(),
                parsed.max_chars,
                duration(),
            ));
        }

        let Some(session_id) = CURRENT_SESSION_ID
            .try_with(|s| s.clone())
            .ok()
            .filter(|s| !s.is_empty())
        else {
            return Ok(ToolResult::failure(
                "no active session — tool.retrieve only works inside a live conversation",
                duration(),
            ));
        };

        let session = match self.sessions.get(&session_id) {
            Ok(Some(s)) => s,
            Ok(None) => return Ok(ToolResult::failure("session not found", duration())),
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("session load failed: {e}"),
                    duration(),
                ))
            }
        };

        // Locate the target tool message. Handle wins; otherwise the most recent
        // message for the named tool that still has a recoverable original.
        let selected = session
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .filter_map(|m| m.tool_data.as_ref())
            .filter(|td| match &parsed.handle {
                Some(h) => td.get("retrieve_handle").and_then(|v| v.as_str()) == Some(h.as_str()),
                None => {
                    let tool = parsed.tool.as_deref().unwrap_or_default();
                    td.get("name").and_then(|v| v.as_str()) == Some(tool)
                }
            })
            .filter_map(original_of)
            .next_back();

        let Some((tool_name, original)) = selected else {
            let what = parsed
                .handle
                .as_deref()
                .map(|h| format!("handle `{h}`"))
                .unwrap_or_else(|| format!("tool `{}`", parsed.tool.as_deref().unwrap_or("?")));
            return Ok(ToolResult::failure(
                format!(
                    "no recoverable original for {what} in this session — it may have been \
                     evicted before retrieval was available, or dropped by the size cap. \
                     Re-run the tool to regenerate it."
                ),
                duration(),
            ));
        };

        Ok(render_retrieved(
            &tool_name,
            &original,
            parsed.query.as_deref(),
            parsed.max_chars,
            duration(),
        ))
    }
}

/// Recover a blob staged by the spill backstop. The handle is an artifact id;
/// `read_session_scoped` answers cross-session / cross-user / expired / missing
/// alike with not-found wording, so an id never leaks existence.
fn retrieve_artifact(
    id: &ArtifactId,
    query: Option<&str>,
    max_chars: Option<usize>,
    duration_ms: u64,
) -> ToolResult {
    let Some(store) = global_store() else {
        return ToolResult::failure("artifact store unavailable", duration_ms);
    };
    let Some(agent_id) = CURRENT_AGENT_ID
        .try_with(|a| a.clone())
        .ok()
        .filter(|a| !a.is_empty())
    else {
        return ToolResult::failure(
            "no active agent — tool.retrieve only works inside a live conversation",
            duration_ms,
        );
    };
    let (body, descriptor) = match read_session_scoped(store.as_ref(), &agent_id, id) {
        Ok(pair) => pair,
        Err(e) => return ToolResult::failure(e.to_string(), duration_ms),
    };
    let producer = descriptor.map_or_else(|| "tool".to_string(), |d| d.producer);
    let original = match body {
        ArtifactBody::Json(v) => value_to_text(&v),
        ArtifactBody::Csv(s) => s,
        ArtifactBody::Bytes(b) => String::from_utf8_lossy(&b).into_owned(),
    };
    render_retrieved(&producer, &original, query, max_chars, duration_ms)
}

/// Egress-scrub the recovered original, apply the optional line query and char
/// cap, and shape the response. Shared by the stash and blob retrieval paths.
fn render_retrieved(
    tool_name: &str,
    original: &str,
    query: Option<&str>,
    max_chars: Option<usize>,
    duration_ms: u64,
) -> ToolResult {
    // The original is untokenised, so scrub credentials exactly as the ingress
    // path would before this re-enters the prompt.
    let safe = tokenize_for_egress(original, tool_name);
    let total_chars = safe.chars().count();
    let cap = max_chars
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(1, HARD_MAX_CHARS);

    // Query filter: keep only matching lines (case-insensitive). Falls back to a
    // head slice with a note when nothing matches.
    let (body, matched_lines, mode) = match query.filter(|q| !q.is_empty()) {
        Some(q) => {
            let needle = q.to_lowercase();
            let lines: Vec<&str> = safe
                .lines()
                .filter(|l| l.to_lowercase().contains(&needle))
                .collect();
            if lines.is_empty() {
                (head(&safe, cap), 0usize, "no_query_match_head")
            } else {
                (head(&lines.join("\n"), cap), lines.len(), "query")
            }
        }
        None => (head(&safe, cap), 0, "head"),
    };
    let truncated = body.chars().count() < total_chars && mode != "query";

    ToolResult::success(
        serde_json::json!({
            "tool": tool_name,
            "total_chars": total_chars,
            "returned_chars": body.chars().count(),
            "matched_lines": matched_lines,
            "truncated": truncated,
            "mode": mode,
            "content": body,
        }),
        duration_ms,
    )
}

/// First `cap` characters of `s` on a char boundary.
fn head(s: &str, cap: usize) -> String {
    match s.char_indices().nth(cap) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use tempfile::TempDir;
    use workforce_agent::artifacts::stage_blob;
    use workforce_agent::artifacts::store::{set_global_store, FsArtifactStore};
    use workforce_agent::runner::{CURRENT_SESSION_ID, CURRENT_SESSION_KEY};
    use workforce_common::session::{InMemorySessionStore, Session};

    /// Install a process-global store backed by a kept-alive TempDir, mirroring
    /// the other hub tests so a `OnceCell` race over the global store is benign:
    /// every test reads back through `global_store()` and namespaces by agent.
    fn install_store() -> Arc<dyn workforce_agent::artifacts::ArtifactStore> {
        static DIR: LazyLock<TempDir> = LazyLock::new(|| {
            let dir = TempDir::new().expect("tempdir");
            set_global_store(
                FsArtifactStore::new(dir.path().to_path_buf()).expect("artifact store"),
            );
            dir
        });
        LazyLock::force(&DIR);
        global_store().expect("store installed")
    }

    fn stage_scoped(
        agent: &str,
        session: &str,
        producer: &str,
        value: serde_json::Value,
    ) -> String {
        let store = install_store();
        let envelope = CURRENT_AGENT_ID.sync_scope(agent.to_string(), || {
            CURRENT_SESSION_ID.sync_scope(session.to_string(), || {
                stage_blob(store.as_ref(), agent, producer, value)
            })
        });
        envelope.expect("stage_blob")["artifact_id"]
            .as_str()
            .expect("artifact_id")
            .to_string()
    }

    fn store_with_evicted_output() -> (Arc<dyn SessionStore>, String) {
        let mut session = Session::new("test", "user", "agent");
        // A big code.search output, then evict it (stashes output_full + handle).
        session.add_tool_message(
            "code.search",
            &serde_json::json!({ "pattern": "needle" }),
            &serde_json::json!(format!(
                "alpha match line\n{}\nzeta needle row 200",
                "filler row\n".repeat(500)
            )),
            true,
            10,
            None,
        );
        session.evict_tool_outputs(Some(&["code.search".to_string()]), None, 0, 500);
        let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        store.save(&session).unwrap();
        (store, session.id.clone())
    }

    fn handle_in_session(store: &Arc<dyn SessionStore>, id: &str) -> String {
        let s = store.get(id).unwrap().unwrap();
        s.messages
            .iter()
            .find_map(|m| {
                m.tool_data
                    .as_ref()?
                    .get("retrieve_handle")?
                    .as_str()
                    .map(str::to_string)
            })
            .expect("evicted message must carry a retrieve_handle")
    }

    #[tokio::test]
    async fn retrieves_full_original_by_handle() {
        let (store, id) = store_with_evicted_output();
        let handle = handle_in_session(&store, &id);
        let tool = RetrieveTool::new(Arc::clone(&store));

        let out = CURRENT_SESSION_ID
            .scope(id, tool.execute(serde_json::json!({ "handle": handle })))
            .await
            .unwrap();

        assert!(out.success, "{:?}", out.error);
        let content = out.output["content"].as_str().unwrap();
        assert!(
            content.contains("zeta needle row 200"),
            "original must be recoverable"
        );
        assert!(out.output["total_chars"].as_u64().unwrap() > 5000);
    }

    #[tokio::test]
    async fn query_returns_only_matching_lines() {
        let (store, id) = store_with_evicted_output();
        let handle = handle_in_session(&store, &id);
        let tool = RetrieveTool::new(Arc::clone(&store));

        let out = CURRENT_SESSION_ID
            .scope(
                id,
                tool.execute(serde_json::json!({ "handle": handle, "query": "needle" })),
            )
            .await
            .unwrap();

        assert!(out.success);
        assert_eq!(out.output["mode"], "query");
        assert_eq!(out.output["matched_lines"], 1); // only "zeta needle row 200" matches
        let content = out.output["content"].as_str().unwrap();
        assert!(content.contains("needle"));
        assert!(!content.contains("filler row"));
    }

    #[tokio::test]
    async fn egress_masks_credentials_in_the_stashed_original() {
        // D2: output_full holds the UNtokenised original. A credential that the
        // ingress layer would have caught must be masked on the way back out.
        // tokenize_for_egress only fires when CURRENT_SESSION_KEY is in scope, so
        // this test scopes BOTH task-locals (ID + KEY) — the gap a SESSION_ID-only
        // test would silently miss.
        let secret = "AKIAIOSFODNN7EXAMPLE"; // matches the AWS_ACCESS_KEY pattern
        let mut session = Session::new("test", "user", "agent");
        session.add_tool_message(
            "code.search",
            &serde_json::json!({ "pattern": "creds" }),
            &serde_json::json!(format!(
                "config dump\nAWS_KEY={secret}\n{}",
                "x".repeat(2000)
            )),
            true,
            10,
            None,
        );
        session.evict_tool_outputs(Some(&["code.search".to_string()]), None, 0, 500);
        let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        store.save(&session).unwrap();

        // Sanity: the raw secret really is sitting in output_full pre-egress.
        let stashed = store.get(&session.id).unwrap().unwrap();
        assert!(stashed.messages.iter().any(|m| m
            .tool_data
            .as_ref()
            .and_then(|td| td.get("output_full"))
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains(secret))));

        let tool = RetrieveTool::new(Arc::clone(&store));
        let fut = tool.execute(serde_json::json!({ "tool": "code.search" }));
        let out = CURRENT_SESSION_KEY
            .scope(
                "test-session-key".to_string(),
                CURRENT_SESSION_ID.scope(session.id.clone(), fut),
            )
            .await
            .unwrap();

        assert!(out.success, "{:?}", out.error);
        let content = out.output["content"].as_str().unwrap();
        assert!(
            !content.contains(secret),
            "the AWS key must be masked on egress, got: {content}"
        );
        assert!(
            content.contains("config dump"),
            "non-secret content should survive"
        );
    }

    #[tokio::test]
    async fn retrieves_by_tool_selector() {
        let (store, id) = store_with_evicted_output();
        let tool = RetrieveTool::new(Arc::clone(&store));
        let out = CURRENT_SESSION_ID
            .scope(
                id,
                tool.execute(serde_json::json!({ "tool": "code.search" })),
            )
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        assert_eq!(out.output["tool"], "code.search");
    }

    #[tokio::test]
    async fn unknown_handle_fails_clearly() {
        let (store, id) = store_with_evicted_output();
        let tool = RetrieveTool::new(Arc::clone(&store));
        let out = CURRENT_SESSION_ID
            .scope(
                id,
                tool.execute(serde_json::json!({ "handle": "h:doesnotexist" })),
            )
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("no recoverable original"));
    }

    #[tokio::test]
    async fn retrieves_spilled_blob_by_artifact_handle() {
        let handle = stage_scoped(
            "blob-agent",
            "blob-sess",
            "datadog.incidents_get",
            serde_json::json!({ "incident": { "title": "S3 creds", "body": "x".repeat(40_000) } }),
        );
        let tool = RetrieveTool::new(Arc::new(InMemorySessionStore::new()));
        let out = CURRENT_AGENT_ID
            .scope(
                "blob-agent".to_string(),
                CURRENT_SESSION_ID.scope(
                    "blob-sess".to_string(),
                    tool.execute(serde_json::json!({ "handle": handle, "query": "S3 creds" })),
                ),
            )
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        assert_eq!(out.output["tool"], "datadog.incidents_get");
        assert_eq!(out.output["mode"], "query");
        assert!(out.output["content"].as_str().unwrap().contains("S3 creds"));
    }

    #[tokio::test]
    async fn artifact_string_blob_returns_inner_text() {
        let handle = stage_scoped(
            "str-agent",
            "str-sess",
            "datadog.api",
            serde_json::json!(format!("plain log head\n{}", "line\n".repeat(2000))),
        );
        let tool = RetrieveTool::new(Arc::new(InMemorySessionStore::new()));
        let out = CURRENT_AGENT_ID
            .scope(
                "str-agent".to_string(),
                CURRENT_SESSION_ID.scope(
                    "str-sess".to_string(),
                    tool.execute(serde_json::json!({ "handle": handle })),
                ),
            )
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        let content = out.output["content"].as_str().unwrap();
        assert!(content.contains("plain log head"));
        assert!(
            !content.contains("\\n"),
            "inner text, not escaped json: {content}"
        );
    }

    #[tokio::test]
    async fn artifact_blob_cross_session_denied() {
        let handle = stage_scoped(
            "xs-agent",
            "sess-1",
            "datadog.incidents_get",
            serde_json::json!({ "k": "x".repeat(40_000) }),
        );
        let tool = RetrieveTool::new(Arc::new(InMemorySessionStore::new()));
        let out = CURRENT_AGENT_ID
            .scope(
                "xs-agent".to_string(),
                CURRENT_SESSION_ID.scope(
                    "sess-2".to_string(),
                    tool.execute(serde_json::json!({ "handle": handle })),
                ),
            )
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("not found or expired"));
    }

    #[tokio::test]
    async fn unknown_artifact_handle_fails_clearly() {
        install_store();
        let fake = ArtifactId::fresh().to_string();
        let tool = RetrieveTool::new(Arc::new(InMemorySessionStore::new()));
        let out = CURRENT_AGENT_ID
            .scope(
                "u-agent".to_string(),
                CURRENT_SESSION_ID.scope(
                    "u-sess".to_string(),
                    tool.execute(serde_json::json!({ "handle": fake })),
                ),
            )
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("not found or expired"));
    }

    #[tokio::test]
    async fn artifact_handle_without_agent_scope_fails() {
        install_store();
        let fake = ArtifactId::fresh().to_string();
        let tool = RetrieveTool::new(Arc::new(InMemorySessionStore::new()));
        // No CURRENT_AGENT_ID in scope → the artifact path can't key the store.
        let out = CURRENT_SESSION_ID
            .scope(
                "s".to_string(),
                tool.execute(serde_json::json!({ "handle": fake })),
            )
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("no active agent"));
    }

    #[tokio::test]
    async fn artifact_blob_egress_masks_credentials() {
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let handle = stage_scoped(
            "cred-agent",
            "cred-sess",
            "datadog.api",
            serde_json::json!(format!(
                "config dump\nAWS_KEY={secret}\n{}",
                "x".repeat(2000)
            )),
        );
        let tool = RetrieveTool::new(Arc::new(InMemorySessionStore::new()));
        let out = CURRENT_SESSION_KEY
            .scope(
                "cred-session-key".to_string(),
                CURRENT_AGENT_ID.scope(
                    "cred-agent".to_string(),
                    CURRENT_SESSION_ID.scope(
                        "cred-sess".to_string(),
                        tool.execute(serde_json::json!({ "handle": handle })),
                    ),
                ),
            )
            .await
            .unwrap();
        assert!(out.success, "{:?}", out.error);
        let content = out.output["content"].as_str().unwrap();
        assert!(
            !content.contains(secret),
            "AWS key must be masked: {content}"
        );
        assert!(content.contains("config dump"));
    }

    #[test]
    fn output_full_key_is_referenced_only_by_session_and_retrieve() {
        // By-construction guard for the D2 security invariant: the UNtokenised
        // original under `output_full` must be read/written by nothing
        // model-facing. Code that touches the key uses the quoted literal
        // `"output_full"`; this fails if it appears outside the two files allowed
        // to handle the stash (the stasher in session.rs, the reader here).
        use std::path::Path;

        fn walk(dir: &Path, needle: &str, allow: &[&str], hits: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, needle, allow, hits);
                } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if allow.contains(&name) {
                        continue;
                    }
                    if std::fs::read_to_string(&p).is_ok_and(|s| s.contains(needle)) {
                        hits.push(p.display().to_string());
                    }
                }
            }
        }

        let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let allow = ["session.rs", "retrieve_tool.rs"];
        let mut hits = Vec::new();
        for c in ["common", "hub", "agent-runtime"] {
            walk(
                &crates_dir.join(c).join("src"),
                "\"output_full\"",
                &allow,
                &mut hits,
            );
        }
        assert!(
            hits.is_empty(),
            "`\"output_full\"` referenced outside the allowlist (possible untokenised-original leak into a model-facing path): {hits:?}"
        );
    }

    #[tokio::test]
    async fn requires_handle_or_tool() {
        let store: Arc<dyn SessionStore> = Arc::new(InMemorySessionStore::new());
        let tool = RetrieveTool::new(store);
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("handle"));
    }
}

#[cfg(test)]
mod surface_tests {
    use std::sync::Arc;

    use workforce_agent::spill::RetrievalSurface;
    use workforce_agent::tools::registry::ToolRegistry;
    use workforce_common::session::InMemorySessionStore;

    use super::RetrieveTool;

    /// Spilling a blob is only worthwhile where this tool can read it back, and
    /// that link is a tool NAME matched in another crate. Drive the real tool
    /// through the real derivation so a rename cannot silently switch blob
    /// spilling off across the hub while the suite stays green.
    #[test]
    fn a_registered_retrieve_tool_grants_the_blob_capability() {
        let registry = ToolRegistry::new();
        assert!(
            !RetrievalSurface::from_registry(&registry).blobs,
            "no retrieval tool ⇒ no blob capability"
        );

        registry.register(Arc::new(RetrieveTool::new(Arc::new(
            InMemorySessionStore::default(),
        ))));
        assert!(
            RetrievalSurface::from_registry(&registry).blobs,
            "a registered RetrieveTool must grant the blob capability — if this \
             fails, its name and spill::RETRIEVE_TOOL_NAME have diverged"
        );
    }
}
