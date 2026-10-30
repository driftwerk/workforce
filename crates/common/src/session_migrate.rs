//! One-shot JSONL → Postgres migration for the session store.
//!
//! Copies every per-agent JSONL session (live transcript + archive) into the
//! shared Postgres backend (after running the refinery migrations against the
//! destination). Driven by the `workforce migrate-sessions` CLI subcommand.
//!
//! ## Design
//!
//! - **Idempotent.** Every insert uses `ON CONFLICT DO NOTHING`, so
//!   re-running is safe and skips already-migrated sessions/archive rows.
//! - **Promoted columns.** `sealed`/`seq`/`prev_session_id` are read off the
//!   session metadata and written into their indexed columns, matching what
//!   live writes do.
//! - **Archive source.** Streams `{id}.archive.jsonl` when present, else the
//!   live `{id}.jsonl` — mirroring `JsonlSessionStore`'s archive seeding.
//! - **Per agent.** One workspace at a time, scoped to its `agent_id`; the
//!   hub's shared sessions store is `agent_id`-scoped the same way.
//!
//! ## Cutover
//!
//! `ON CONFLICT DO NOTHING` will not re-pick a session modified after its
//! first insert, so writes must be quiesced during/after backfill (stop the
//! hub, migrate, set `[sessions] backend = "postgres"`, restart).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use eyre::{Context, Result};
use uuid::Uuid;

use crate::pg_text::{strip_nuls_json, strip_nuls_str};
use crate::postgres::{PostgresConfig, PostgresPool};
use crate::session::{JsonlSessionStore, MessageRole, Session, SessionMessage, SessionStore};
use crate::session_postgres::PostgresSessionStore;

/// Per-agent row-count summary returned to the caller for reporting.
#[derive(Debug, Default, Clone)]
pub struct SessionMigrationReport {
    pub sessions: usize,
    pub archive_messages: usize,
    /// Sessions inserted sealed because a newer unsealed session claimed the
    /// same `(channel, owner)` — Postgres allows one active per conversation.
    pub force_sealed: usize,
    /// Sessions whose live blob was cut to the newest messages fitting
    /// [`MigrateOptions::live_blob_cap_bytes`]; their archive is complete.
    pub truncated: usize,
    /// Session ids that failed to copy. The run continues past them; a rerun
    /// after fixing the source is idempotent.
    pub failed: Vec<String>,
}

impl SessionMigrationReport {
    fn add(&mut self, other: Self) {
        self.sessions += other.sessions;
        self.archive_messages += other.archive_messages;
        self.force_sealed += other.force_sealed;
        self.truncated += other.truncated;
        self.failed.extend(other.failed);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MigrateOptions {
    /// Ceiling on a session's encoded live `messages` blob. A pathological
    /// multi-hundred-MB session as a single jsonb insert can OOM the server;
    /// the archive keeps the full history either way.
    pub live_blob_cap_bytes: usize,
}

impl Default for MigrateOptions {
    fn default() -> Self {
        Self {
            live_blob_cap_bytes: 32 * 1024 * 1024,
        }
    }
}

/// Migrate every agent workspace's JSONL sessions into Postgres.
///
/// `workspaces` is `(agent_id, workspace_dir)` pairs. Builds a fresh
/// sessions pool from `pg_config` (search_path pinned), runs the destination
/// migrations once, and copies each workspace under its `agent_id`.
pub async fn migrate_sessions_to_postgres(
    workspaces: &[(String, std::path::PathBuf)],
    pg_config: &PostgresConfig,
) -> Result<SessionMigrationReport> {
    let pool = PostgresSessionStore::build_pool(pg_config)
        .context("building sessions pool for migration destination")?;
    crate::session_postgres::run_session_migrations(&pool)
        .await
        .context("preparing Postgres destination")?;

    let mut total = SessionMigrationReport::default();
    for (agent_id, workspace) in workspaces {
        let report = migrate_workspace_with_pool(
            agent_id,
            workspace,
            pool.clone(),
            MigrateOptions::default(),
        )
        .await?;
        total.add(report);
    }
    Ok(total)
}

/// Migrate a single workspace into an already-migrated pool. The pool MUST
/// come from [`PostgresSessionStore::build_pool`] (search_path pinned). Used
/// by tests and by the multi-workspace driver above.
pub async fn migrate_workspace_with_pool(
    agent_id: &str,
    workspace: &Path,
    pool: PostgresPool,
    options: MigrateOptions,
) -> Result<SessionMigrationReport> {
    // The file store enumerates session shells and hydrates bodies on demand;
    // reuse it as the reader rather than re-parsing the on-disk layout.
    let src = JsonlSessionStore::new(workspace).context("opening JSONL source")?;
    let shells = src.list(None, usize::MAX).context("listing sessions")?;
    let force_seal = force_seal_ids(&shells);

    let mut report = SessionMigrationReport::default();
    for shell in shells {
        match copy_session(&pool, agent_id, &src, &shell.id, &force_seal, options).await {
            Ok(Some(copied)) => {
                report.sessions += usize::from(copied.inserted);
                report.archive_messages += copied.archive_rows;
                report.force_sealed += usize::from(copied.force_sealed);
                report.truncated += usize::from(copied.truncated);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(session_id = %shell.id, error = %e, "session copy failed — continuing");
                report.failed.push(shell.id.clone());
            }
        }
    }
    Ok(report)
}

struct CopiedSession {
    inserted: bool,
    archive_rows: usize,
    force_sealed: bool,
    truncated: bool,
}

async fn copy_session(
    pool: &PostgresPool,
    agent_id: &str,
    src: &JsonlSessionStore,
    session_id: &str,
    force_seal: &HashSet<String>,
    options: MigrateOptions,
) -> Result<Option<CopiedSession>> {
    let Some(session) = src.get(session_id)? else {
        return Ok(None);
    };
    let force_sealed = force_seal.contains(session_id);
    let (inserted, truncated) =
        insert_session(pool, agent_id, &session, force_sealed, options).await?;
    let archive_rows = insert_archive(pool, src, &session).await?;
    Ok(Some(CopiedSession {
        inserted,
        archive_rows,
        force_sealed: inserted && force_sealed,
        truncated,
    }))
}

/// Ids of unsealed sessions that must be inserted sealed: for each
/// `(channel, owner)` with more than one unsealed session, everything but
/// the newest (by `updated_at`, id as tiebreak) — the partial-unique index
/// on the destination allows exactly one active session per conversation.
fn force_seal_ids(shells: &[Session]) -> HashSet<String> {
    let mut groups: HashMap<(&str, &str), Vec<&Session>> = HashMap::new();
    for shell in shells.iter().filter(|s| !s.is_sealed()) {
        groups
            .entry((shell.channel.as_str(), shell.owner.as_str()))
            .or_default()
            .push(shell);
    }
    groups
        .into_values()
        .filter(|members| members.len() > 1)
        .flat_map(|mut members| {
            // Seq first: the file stores pick the active session by highest
            // seq, so the survivor must match what `find` would return.
            members.sort_by(|a, b| {
                (a.seq(), a.updated_at, a.id.as_str()).cmp(&(b.seq(), b.updated_at, b.id.as_str()))
            });
            members.pop();
            members.into_iter().map(|s| s.id.clone())
        })
        .collect()
}

fn role_to_str(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
        MessageRole::Tool => "tool",
    }
}

/// Keep the newest run of messages whose combined encoded size fits `cap`.
/// Returns the retained slice start (0 = everything kept).
fn live_blob_keep_from(messages: &[SessionMessage], cap: usize) -> Result<usize> {
    let mut budget = cap;
    for (idx, msg) in messages.iter().enumerate().rev() {
        let size = serde_json::to_string(msg).context("encode message")?.len();
        if size > budget {
            return Ok(idx + 1);
        }
        budget -= size;
    }
    Ok(0)
}

/// Insert one session row, scrubbed of NULs, sealed when `force_seal`, and
/// with the live blob capped to `options.live_blob_cap_bytes`. Returns
/// `(inserted, truncated)`; an already-present row inserts nothing.
async fn insert_session(
    pool: &PostgresPool,
    agent_id: &str,
    session: &Session,
    force_seal: bool,
    options: MigrateOptions,
) -> Result<(bool, bool)> {
    let id = Uuid::parse_str(&session.id)
        .with_context(|| format!("invalid session UUID: {}", session.id))?;
    let prev = session
        .prev_session_id()
        .map(Uuid::parse_str)
        .transpose()
        .with_context(|| format!("invalid prev_session_id on {}", session.id))?;

    let keep_from = live_blob_keep_from(&session.messages, options.live_blob_cap_bytes)?;
    let truncated = keep_from > 0;
    // An unsealed session cut to NOTHING (its newest message alone exceeds
    // the cap) must not stay active with zero context — seal it, so the next
    // message starts a fresh session while the archive keeps the history.
    let force_seal =
        force_seal || (truncated && keep_from == session.messages.len() && !session.is_sealed());
    if truncated {
        tracing::warn!(
            session_id = %session.id,
            dropped = keep_from,
            kept = session.messages.len() - keep_from,
            sealed = force_seal,
            "live blob over cap — keeping only the newest messages (archive keeps everything)"
        );
    }
    let kept = &session.messages[keep_from..];
    let mut messages = serde_json::to_value(kept).context("encode messages")?;
    strip_nuls_json(&mut messages);
    let mut metadata = session.metadata.clone();
    strip_nuls_json(&mut metadata);
    if force_seal {
        // The sealed COLUMN is derived from `metadata.sealed` on every live
        // write and never flows back on load — both must say sealed, or the
        // loaded session would report itself active again.
        if !metadata.is_object() {
            metadata = serde_json::json!({});
        }
        metadata
            .as_object_mut()
            .expect("metadata forced to an object above")
            .insert("sealed".into(), serde_json::json!(true));
    }
    let mut compaction =
        serde_json::to_value(&session.compaction_history).context("encode compaction history")?;
    strip_nuls_json(&mut compaction);
    let total_tokens: i64 = if truncated {
        kept.iter().map(|m| i64::from(m.tokens)).sum()
    } else {
        session.total_tokens as i64
    };
    let context_type = session.context_type.as_ref().map(|ct| ct.to_string());
    let kind = serde_json::to_string(&session.kind)
        .ok()
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_else(|| "main".to_string());

    let conn = pool.get().await?;
    let affected = conn
        .execute(
            "INSERT INTO sessions
             (id, agent_id, channel, owner, label, kind, context_type, created_at, updated_at,
              total_tokens, message_count, model_override, compaction_history, metadata, messages,
              sealed, seq, prev_session_id, revision)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                     $18, 0)
             ON CONFLICT (id) DO NOTHING",
            &[
                &id,
                &agent_id,
                &session.channel,
                &session.owner,
                &session.label,
                &kind,
                &context_type,
                &session.created_at,
                &session.updated_at,
                &total_tokens,
                &(kept.len() as i32),
                &session.model_override,
                &compaction,
                &metadata,
                &messages,
                &(session.is_sealed() || force_seal),
                &(session.seq() as i64),
                &prev,
            ],
        )
        .await?;
    Ok((affected > 0, truncated))
}

/// Rows per batched archive insert — bounds statement size and server-side
/// memory for very long transcripts.
const ARCHIVE_BATCH: usize = 500;

/// Byte ceiling per batched insert. Row count alone is not enough: 500
/// attachment-heavy messages can build a statement large enough to drop the
/// connection (a 141MB archive did exactly that through the RDS proxy). An
/// oversized single message still ships alone in its own statement.
const ARCHIVE_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// Approximate encoded size without allocating — only batching needs it.
fn json_size_estimate(v: &serde_json::Value) -> usize {
    use serde_json::Value;
    match v {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(_) => 20,
        Value::String(s) => s.len() + 2,
        Value::Array(a) => 2 + a.iter().map(json_size_estimate).sum::<usize>(),
        Value::Object(o) => {
            2 + o
                .iter()
                .map(|(k, val)| k.len() + 4 + json_size_estimate(val))
                .sum::<usize>()
        }
    }
}

/// Split messages into runs of at most [`ARCHIVE_BATCH`] rows AND (except for
/// a single oversized message) at most [`ARCHIVE_BATCH_BYTES`] encoded bytes.
fn archive_batches(messages: &[SessionMessage]) -> Vec<&[SessionMessage]> {
    let mut batches = Vec::new();
    let mut start = 0usize;
    let mut bytes = 0usize;
    for (idx, msg) in messages.iter().enumerate() {
        let size = msg.content.len() + msg.tool_data.as_ref().map(json_size_estimate).unwrap_or(0);
        let full =
            idx > start && (bytes + size > ARCHIVE_BATCH_BYTES || idx - start >= ARCHIVE_BATCH);
        if full {
            batches.push(&messages[start..idx]);
            start = idx;
            bytes = 0;
        }
        bytes += size;
    }
    if start < messages.len() {
        batches.push(&messages[start..]);
    }
    batches
}

/// Insert this session's archive rows in batches. Idempotent on the
/// `(session_id, message_id)` unique index (V004): a re-run — or a duplicate
/// within the source — inserts nothing. Returns the number of new rows.
async fn insert_archive(
    pool: &PostgresPool,
    src: &JsonlSessionStore,
    session: &Session,
) -> Result<usize> {
    let id = Uuid::parse_str(&session.id)
        .with_context(|| format!("invalid session UUID: {}", session.id))?;
    // A crash-doubled source line would otherwise duplicate a message id
    // inside one batched INSERT; keep the first occurrence.
    let mut seen = HashSet::new();
    let messages: Vec<SessionMessage> = read_archive_messages(src, session)?
        .into_iter()
        .filter(|m| seen.insert(m.id.clone()))
        .collect();
    if messages.is_empty() {
        return Ok(0);
    }
    let conn = pool.get().await?;
    let mut inserted = 0usize;
    for batch in archive_batches(&messages) {
        let session_ids = vec![id; batch.len()];
        let message_ids: Vec<&str> = batch.iter().map(|m| m.id.as_str()).collect();
        let roles: Vec<&str> = batch.iter().map(|m| role_to_str(&m.role)).collect();
        let contents: Vec<String> = batch
            .iter()
            .map(|m| strip_nuls_str(&m.content).into_owned())
            .collect();
        let tool_data: Vec<Option<serde_json::Value>> = batch
            .iter()
            .map(|m| {
                m.tool_data.clone().map(|mut v| {
                    strip_nuls_json(&mut v);
                    v
                })
            })
            .collect();
        let timestamps: Vec<_> = batch.iter().map(|m| m.timestamp).collect();
        let tokens: Vec<i32> = batch.iter().map(|m| m.tokens as i32).collect();
        inserted += conn
            .execute(
                "INSERT INTO session_archive
                 (session_id, message_id, role, content, tool_data, ts, tokens)
                 SELECT * FROM UNNEST($1::uuid[], $2::text[], $3::text[], $4::text[],
                                      $5::jsonb[], $6::timestamptz[], $7::int4[])
                 ON CONFLICT (session_id, message_id) DO NOTHING",
                &[
                    &session_ids,
                    &message_ids,
                    &roles,
                    &contents,
                    &tool_data,
                    &timestamps,
                    &tokens,
                ],
            )
            .await? as usize;
    }
    Ok(inserted)
}

/// Read the full archive for a session via the file store. Walks the archive
/// page-by-page so it never needs a private accessor; `read_archive_page`
/// falls back to the live transcript when no archive file exists yet,
/// matching `JsonlSessionStore`'s seed behaviour.
fn read_archive_messages(
    src: &JsonlSessionStore,
    session: &Session,
) -> Result<Vec<SessionMessage>> {
    let page = SessionStore::read_archive_page(src, &session.id, None, usize::MAX)?;
    Ok(page
        .messages
        .into_iter()
        .map(|e| SessionMessage {
            id: e.id,
            role: e.role,
            content: e.content,
            tool_data: e.tool_metadata.map(|tm| {
                serde_json::json!({
                    "name": tm.name,
                    "input": tm.input,
                    "output": tm.output,
                    "duration_ms": tm.duration_ms,
                    "success": tm.success,
                })
            }),
            timestamp: e.timestamp,
            tokens: e.token_usage.map(|u| u.output_tokens).unwrap_or(0),
            external_id: None,
            attachments: None,
            trust_level: None,
            actor: None,
            desk: None,
        })
        .collect())
}

/// Variant taking a pre-built pool for a single workspace, used by tests.
pub async fn migrate_workspace(
    agent_id: &str,
    workspace: &Path,
    pg_config: &PostgresConfig,
) -> Result<SessionMigrationReport> {
    let pool = PostgresSessionStore::build_pool(pg_config)
        .context("building sessions pool for migration destination")?;
    let _store: Arc<PostgresSessionStore> = PostgresSessionStore::with_pool(pool.clone(), agent_id)
        .await
        .context("preparing Postgres destination")?;
    migrate_workspace_with_pool(agent_id, workspace, pool, MigrateOptions::default()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_postgres::tests::with_embedded_pg_pool;

    #[tokio::test(flavor = "multi_thread")]
    async fn backfill_is_idempotent() {
        with_embedded_pg_pool(|pool| async move {
            // Seed a JSONL workspace with two sessions, one sealed with a
            // predecessor link and an archive.
            let tmp = tempfile::tempdir().unwrap();
            let workspace = tmp.path();
            {
                let src: Arc<dyn SessionStore> =
                    Arc::new(JsonlSessionStore::new(workspace).unwrap());
                let mut a = src.get_or_create("slack:C", "u", "agent-a").unwrap();
                a.add_user_message("first turn", None);
                src.save(&a).unwrap();
                for m in &a.messages {
                    src.append_to_archive(&a.id, m).unwrap();
                }
                // Seal `a` and mint a successor `b` linked via prev_session_id.
                let mut b = Session::new("slack:C", "u", "agent-a");
                b.set_seq(1);
                b.set_prev_session_id(&a.id);
                b.add_user_message("successor turn", None);
                let mut sealed = src.get(&a.id).unwrap().unwrap();
                sealed.seal();
                src.seal_and_insert_successor(&sealed, &b).unwrap();
                for m in &b.messages {
                    src.append_to_archive(&b.id, m).unwrap();
                }
            }

            // Run migrations once, then migrate the workspace.
            crate::session_postgres::run_session_migrations(&pool)
                .await
                .unwrap();
            let report = migrate_workspace_with_pool(
                "agent-a",
                workspace,
                pool.clone(),
                MigrateOptions::default(),
            )
            .await
            .unwrap();
            assert_eq!(report.sessions, 2);
            assert_eq!(report.archive_messages, 2);
            assert_eq!(report.force_sealed, 0);
            assert_eq!(report.truncated, 0);
            assert!(report.failed.is_empty());

            // Verify the data is queryable through the destination store.
            let dest: Arc<dyn SessionStore> =
                PostgresSessionStore::with_shared_pool(pool.clone(), "agent-a");
            let listed = dest.list(None, 10).unwrap();
            assert_eq!(listed.len(), 2);
            let active = dest.find("slack:C", "u").unwrap().expect("active session");
            // The successor (unsealed) is the active one.
            assert_eq!(active.seq(), 1);
            let thread = dest.read_thread_page(&active.id, None, None, 10).unwrap();
            let contents: Vec<&str> = thread.messages.iter().map(|m| m.content.as_str()).collect();
            assert_eq!(contents, vec!["first turn", "successor turn"]);

            // Re-running yields one copy — sessions skipped, archive skipped.
            let report2 = migrate_workspace_with_pool(
                "agent-a",
                workspace,
                pool.clone(),
                MigrateOptions::default(),
            )
            .await
            .unwrap();
            assert_eq!(report2.sessions, 0, "no new sessions on re-run");
            assert_eq!(report2.archive_messages, 0, "no new archive rows on re-run");
            assert_eq!(dest.list(None, 10).unwrap().len(), 2, "still one copy");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nul_bytes_are_scrubbed() {
        with_embedded_pg_pool(|pool| async move {
            let tmp = tempfile::tempdir().unwrap();
            let workspace = tmp.path();
            {
                let src: Arc<dyn SessionStore> =
                    Arc::new(JsonlSessionStore::new(workspace).unwrap());
                let mut s = src.get_or_create("cli", "u", "agent-a").unwrap();
                s.add_user_message("nul\0inside", None);
                src.save(&s).unwrap();
                for m in &s.messages {
                    src.append_to_archive(&s.id, m).unwrap();
                }
            }
            crate::session_postgres::run_session_migrations(&pool)
                .await
                .unwrap();
            let report = migrate_workspace_with_pool(
                "agent-a",
                workspace,
                pool.clone(),
                MigrateOptions::default(),
            )
            .await
            .unwrap();
            assert_eq!(report.sessions, 1);
            assert!(report.failed.is_empty());

            let dest: Arc<dyn SessionStore> =
                PostgresSessionStore::with_shared_pool(pool, "agent-a");
            let got = dest.list(None, 10).unwrap();
            let full = dest.get(&got[0].id).unwrap().unwrap();
            assert_eq!(full.messages[0].content, "nulinside");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_unsealed_sessions_are_force_sealed() {
        with_embedded_pg_pool(|pool| async move {
            let tmp = tempfile::tempdir().unwrap();
            let workspace = tmp.path();
            let (older_id, newer_id) = {
                let src: Arc<dyn SessionStore> =
                    Arc::new(JsonlSessionStore::new(workspace).unwrap());
                // Two unsealed sessions for one (channel, owner): the JSONL
                // store tolerates this, the Postgres partial-unique does not.
                let mut older = Session::new("slack:C", "u", "agent-a");
                older.add_user_message("older", None);
                older.updated_at = chrono::Utc::now() - chrono::Duration::hours(2);
                src.save(&older).unwrap();
                let mut newer = Session::new("slack:C", "u", "agent-a");
                newer.add_user_message("newer", None);
                src.save(&newer).unwrap();
                (older.id.clone(), newer.id.clone())
            };
            crate::session_postgres::run_session_migrations(&pool)
                .await
                .unwrap();
            let report = migrate_workspace_with_pool(
                "agent-a",
                workspace,
                pool.clone(),
                MigrateOptions::default(),
            )
            .await
            .unwrap();
            assert_eq!(report.sessions, 2);
            assert_eq!(report.force_sealed, 1);
            assert!(report.failed.is_empty());

            let dest: Arc<dyn SessionStore> =
                PostgresSessionStore::with_shared_pool(pool, "agent-a");
            let active = dest.find("slack:C", "u").unwrap().expect("one active");
            assert_eq!(active.id, newer_id, "newest stays unsealed");
            assert!(dest.get(&older_id).unwrap().unwrap().is_sealed());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_live_blob_is_truncated_but_archived_fully() {
        with_embedded_pg_pool(|pool| async move {
            let tmp = tempfile::tempdir().unwrap();
            let workspace = tmp.path();
            let session_id = {
                let src: Arc<dyn SessionStore> =
                    Arc::new(JsonlSessionStore::new(workspace).unwrap());
                let mut s = src.get_or_create("cli", "u", "agent-a").unwrap();
                for i in 0..5 {
                    s.add_user_message(&format!("message number {i} {}", "x".repeat(200)), None);
                }
                src.save(&s).unwrap();
                for m in &s.messages {
                    src.append_to_archive(&s.id, m).unwrap();
                }
                s.id.clone()
            };
            crate::session_postgres::run_session_migrations(&pool)
                .await
                .unwrap();
            let report = migrate_workspace_with_pool(
                "agent-a",
                workspace,
                pool.clone(),
                MigrateOptions {
                    live_blob_cap_bytes: 900,
                },
            )
            .await
            .unwrap();
            assert_eq!(report.truncated, 1);
            assert_eq!(report.archive_messages, 5, "archive keeps everything");

            let dest: Arc<dyn SessionStore> =
                PostgresSessionStore::with_shared_pool(pool, "agent-a");
            let live = dest.get(&session_id).unwrap().unwrap();
            assert!(live.messages.len() < 5, "live blob was cut");
            assert!(
                live.messages.last().unwrap().content.contains("number 4"),
                "newest message survives"
            );
        })
        .await;
    }

    #[test]
    fn archive_batches_bound_rows_and_bytes() {
        let small: Vec<SessionMessage> = (0..3)
            .map(|i| {
                let mut m = crate::session::SessionMessage {
                    id: format!("m{i}"),
                    role: MessageRole::User,
                    content: "tiny".into(),
                    tool_data: None,
                    timestamp: chrono::Utc::now(),
                    tokens: 1,
                    external_id: None,
                    attachments: None,
                    trust_level: None,
                    actor: None,
                    desk: None,
                };
                if i == 1 {
                    m.content = "x".repeat(ARCHIVE_BATCH_BYTES + 1);
                }
                m
            })
            .collect();
        let batches = archive_batches(&small);
        // tiny | oversized-alone | tiny
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[1].len(), 1, "oversized message ships alone");
        assert_eq!(
            batches.iter().map(|b| b.len()).sum::<usize>(),
            3,
            "no message dropped"
        );
    }

    #[test]
    fn force_seal_survivor_matches_find_semantics() {
        // The file stores pick the active session by highest seq, so the
        // higher-seq session survives even when touched less recently.
        let mut older_seq = Session::new("slack:C", "u", "agent-a");
        older_seq.updated_at = chrono::Utc::now();
        let mut newer_seq = Session::new("slack:C", "u", "agent-a");
        newer_seq.set_seq(1);
        newer_seq.updated_at = chrono::Utc::now() - chrono::Duration::hours(3);
        let sealed_ids = force_seal_ids(&[older_seq.clone(), newer_seq.clone()]);
        assert_eq!(sealed_ids.len(), 1);
        assert!(sealed_ids.contains(&older_seq.id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fully_truncated_session_is_sealed_not_left_empty_active() {
        with_embedded_pg_pool(|pool| async move {
            let tmp = tempfile::tempdir().unwrap();
            let workspace = tmp.path();
            let session_id = {
                let src: Arc<dyn SessionStore> =
                    Arc::new(JsonlSessionStore::new(workspace).unwrap());
                let mut s = src.get_or_create("cli", "u", "agent-a").unwrap();
                s.add_user_message(&"x".repeat(500), None);
                src.save(&s).unwrap();
                for m in &s.messages {
                    src.append_to_archive(&s.id, m).unwrap();
                }
                s.id.clone()
            };
            crate::session_postgres::run_session_migrations(&pool)
                .await
                .unwrap();
            let report = migrate_workspace_with_pool(
                "agent-a",
                workspace,
                pool.clone(),
                MigrateOptions {
                    live_blob_cap_bytes: 100,
                },
            )
            .await
            .unwrap();
            assert_eq!(report.truncated, 1);
            assert_eq!(report.archive_messages, 1);

            let dest: Arc<dyn SessionStore> =
                PostgresSessionStore::with_shared_pool(pool, "agent-a");
            let stored = dest.get(&session_id).unwrap().unwrap();
            assert!(stored.messages.is_empty());
            assert!(stored.is_sealed(), "empty live blob must not stay active");
            assert!(dest.find("cli", "u").unwrap().is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_session_does_not_stop_the_run() {
        with_embedded_pg_pool(|pool| async move {
            let tmp = tempfile::tempdir().unwrap();
            let workspace = tmp.path();
            let good_channel = {
                let src: Arc<dyn SessionStore> =
                    Arc::new(JsonlSessionStore::new(workspace).unwrap());
                let mut good = src.get_or_create("cli", "u", "agent-a").unwrap();
                good.add_user_message("fine", None);
                src.save(&good).unwrap();
                // A session whose id is not a UUID cannot land in Postgres —
                // the copy must fail for it alone.
                let mut bad = Session::new("cli-bad", "u", "agent-a");
                bad.id = "not-a-uuid".into();
                bad.add_user_message("broken", None);
                src.save(&bad).unwrap();
                good.channel.clone()
            };
            crate::session_postgres::run_session_migrations(&pool)
                .await
                .unwrap();
            let report = migrate_workspace_with_pool(
                "agent-a",
                workspace,
                pool.clone(),
                MigrateOptions::default(),
            )
            .await
            .unwrap();
            assert_eq!(report.sessions, 1, "good session copied");
            assert_eq!(report.failed, vec!["not-a-uuid".to_string()]);

            let dest: Arc<dyn SessionStore> =
                PostgresSessionStore::with_shared_pool(pool, "agent-a");
            assert!(dest.find(&good_channel, "u").unwrap().is_some());
        })
        .await;
    }
}
