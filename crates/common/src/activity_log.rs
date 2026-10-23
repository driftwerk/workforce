//! Activity history queries against the telemetry tables.
//!
//! Defines the public [`ActivityRecord`] / [`ActivityQuery`] /
//! [`ActivityType`] types plus the inherent [`SqliteStore`] methods
//! that join `task_telemetry`, `tool_telemetry`, and `round_telemetry`
//! into a unified activity stream.
//!
//! Tool and round entries carry their *task's* `started_at` as the
//! timestamp — the telemetry schema records no per-row execution times —
//! so a task's tools cluster at its start; `round N` in the description
//! gives the in-task order. The high-level entry points
//! ([`SqliteStore::query_activities`] and
//! [`SqliteStore::prune_activity`]) are also exposed via the
//! backend-agnostic [`crate::telemetry::TelemetryStore`] trait so
//! consumers don't need a concrete SQLite handle.

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use eyre::{eyre, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::persistence::SqliteStore;
use crate::telemetry::{prune_in_batches, PRUNE_BATCH};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityType {
    SessionStarted,
    SessionCompleted,
    ToolExecution,
    LlmCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityRecord {
    pub timestamp: DateTime<Utc>,
    pub agent_id: String,
    pub activity_type: ActivityType,
    pub channel: Option<String>,
    pub description: String,
    pub duration_ms: Option<i64>,
    pub success: Option<bool>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Default)]
pub struct ActivityQuery {
    pub agent_id: Option<String>,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub verbose: bool,
}

fn parse_rfc3339(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn channel_from_session(session_id: &Option<String>) -> Option<String> {
    session_id
        .as_ref()
        .and_then(|s| s.split(':').next().map(String::from))
}

impl SqliteStore {
    /// Join task / tool / round telemetry into a unified activity
    /// stream, ordered chronologically. Verbose queries also include
    /// per-round LLM call entries.
    pub fn query_activities(&self, query: &ActivityQuery) -> Result<Vec<ActivityRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| eyre!("Lock poisoned: {}", e))?;
        let mut activities = query_tasks(&conn, query)?;
        activities.extend(query_tools(&conn, query)?);
        if query.verbose {
            activities.extend(query_rounds(&conn, query)?);
        }
        activities.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        Ok(activities)
    }

    /// Delete every task that completed before `before`, with its rounds
    /// and tool calls (SQLite enforces no cascade here), a batch per
    /// transaction so a busy hub's writers wait for one batch at most.
    pub fn prune_activity(&self, before: DateTime<Utc>) -> Result<usize> {
        let before = before.to_rfc3339();
        prune_in_batches(|| {
            let mut conn = self
                .conn
                .lock()
                .map_err(|e| eyre!("Lock poisoned: {}", e))?;
            let tx = conn.transaction()?;
            let ids: String = tx.query_row(
                "SELECT json_group_array(id) FROM (SELECT id FROM task_telemetry
                 WHERE completed_at < ?1 ORDER BY completed_at LIMIT ?2)",
                params![before, PRUNE_BATCH as i64],
                |row| row.get(0),
            )?;
            for child in ["round_telemetry", "tool_telemetry"] {
                tx.execute(
                    &format!(
                        "DELETE FROM {child} WHERE task_telemetry_id IN (SELECT value FROM json_each(?1))"
                    ),
                    params![ids],
                )?;
            }
            let batch = tx.execute(
                "DELETE FROM task_telemetry WHERE id IN (SELECT value FROM json_each(?1))",
                params![ids],
            )?;
            tx.commit()?;
            Ok(batch)
        })
    }
}

fn query_tasks(conn: &Connection, query: &ActivityQuery) -> Result<Vec<ActivityRecord>> {
    let sql = "SELECT
        t.agent_id,
        t.started_at,
        t.completed_at,
        t.session_id,
        (t.input_tokens + t.output_tokens) as total_tokens,
        t.cost_usd,
        t.rounds,
        t.success,
        t.error_type,
        t.model,
        t.duration_ms
    FROM task_telemetry t
    WHERE 1=1
        AND (?1 IS NULL OR t.agent_id = ?1)
        AND (?2 IS NULL OR t.started_at >= ?2)
        AND (?3 IS NULL OR t.started_at <= ?3)
    ORDER BY t.started_at DESC";

    let start_str = query.start_time.map(|t| t.to_rfc3339());
    let end_str = query.end_time.map(|t| t.to_rfc3339());

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![
            query.agent_id.as_deref(),
            start_str.as_deref(),
            end_str.as_deref(),
        ],
        |row| {
            let agent_id: String = row.get(0)?;
            let started_at_str: String = row.get(1)?;
            let completed_at_str: String = row.get(2)?;
            let session_id: Option<String> = row.get(3)?;
            let total_tokens: i64 = row.get(4)?;
            let cost_usd: f64 = row.get(5)?;
            let rounds: i64 = row.get(6)?;
            let success: bool = row.get(7)?;
            let error_type: Option<String> = row.get(8)?;
            let model: String = row.get(9)?;
            let duration_ms: i64 = row.get(10)?;

            let started_at = parse_rfc3339(&started_at_str);
            let completed_at = DateTime::parse_from_rfc3339(&completed_at_str)
                .map(|dt| dt.with_timezone(&Utc))
                .ok();
            let channel = channel_from_session(&session_id);

            let mut records = vec![];

            records.push(ActivityRecord {
                timestamp: started_at,
                agent_id: agent_id.clone(),
                activity_type: ActivityType::SessionStarted,
                channel: channel.clone(),
                description: format!("Task started on {} ({} rounds)", model, rounds),
                duration_ms: None,
                success: None,
                metadata: None,
            });

            if let Some(completed) = completed_at {
                let outcome = if success {
                    "success".to_string()
                } else {
                    error_type
                        .as_ref()
                        .map(|e| format!("failed: {e}"))
                        .unwrap_or_else(|| "failed".to_string())
                };

                let mut metadata = serde_json::Map::new();
                metadata.insert(
                    "total_tokens".to_string(),
                    serde_json::Value::Number(total_tokens.into()),
                );
                metadata.insert("cost_usd".to_string(), serde_json::Value::from(cost_usd));
                metadata.insert(
                    "model".to_string(),
                    serde_json::Value::String(model.clone()),
                );

                records.push(ActivityRecord {
                    timestamp: completed,
                    agent_id,
                    activity_type: ActivityType::SessionCompleted,
                    channel,
                    description: format!(
                        "Task completed ({}) — {} tokens, ${:.4}",
                        outcome, total_tokens, cost_usd
                    ),
                    duration_ms: Some(duration_ms),
                    success: Some(success),
                    metadata: Some(serde_json::Value::Object(metadata)),
                });
            }

            Ok(records)
        },
    )?;

    let mut activities = Vec::new();
    for row_result in rows {
        activities.extend(row_result?);
    }
    Ok(activities)
}

fn query_tools(conn: &Connection, query: &ActivityQuery) -> Result<Vec<ActivityRecord>> {
    let sql = "SELECT
        tool.tool_name,
        tool.success,
        tool.duration_ms,
        tool.round_number,
        task.agent_id,
        task.started_at,
        task.session_id
    FROM tool_telemetry tool
    JOIN task_telemetry task ON tool.task_telemetry_id = task.id
    WHERE 1=1
        AND (?1 IS NULL OR task.agent_id = ?1)
        AND (?2 IS NULL OR task.started_at >= ?2)
        AND (?3 IS NULL OR task.started_at <= ?3)
    ORDER BY task.started_at DESC, tool.round_number ASC";

    let start_str = query.start_time.map(|t| t.to_rfc3339());
    let end_str = query.end_time.map(|t| t.to_rfc3339());

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![
            query.agent_id.as_deref(),
            start_str.as_deref(),
            end_str.as_deref(),
        ],
        |row| {
            let tool_name: String = row.get(0)?;
            let success: bool = row.get(1)?;
            let duration_ms: i64 = row.get(2)?;
            let round_number: i64 = row.get(3)?;
            let agent_id: String = row.get(4)?;
            let started_at_str: String = row.get(5)?;
            let session_id: Option<String> = row.get(6)?;

            let timestamp = parse_rfc3339(&started_at_str);
            let channel = channel_from_session(&session_id);
            let status = if success { "" } else { " [FAILED]" };

            Ok(ActivityRecord {
                timestamp,
                agent_id,
                activity_type: ActivityType::ToolExecution,
                channel,
                description: format!(
                    "Tool: {}{} ({}ms, round {})",
                    tool_name, status, duration_ms, round_number
                ),
                duration_ms: Some(duration_ms),
                success: Some(success),
                metadata: None,
            })
        },
    )?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn query_rounds(conn: &Connection, query: &ActivityQuery) -> Result<Vec<ActivityRecord>> {
    let sql = "SELECT
        r.round_number,
        r.input_tokens,
        r.output_tokens,
        r.latency_ms,
        r.cache_read_tokens,
        r.tool_count,
        r.stop_reason,
        t.agent_id,
        t.started_at,
        t.session_id,
        t.model
    FROM round_telemetry r
    JOIN task_telemetry t ON r.task_telemetry_id = t.id
    WHERE 1=1
        AND (?1 IS NULL OR t.agent_id = ?1)
        AND (?2 IS NULL OR t.started_at >= ?2)
        AND (?3 IS NULL OR t.started_at <= ?3)
    ORDER BY t.started_at DESC, r.round_number ASC";

    let start_str = query.start_time.map(|t| t.to_rfc3339());
    let end_str = query.end_time.map(|t| t.to_rfc3339());

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![
            query.agent_id.as_deref(),
            start_str.as_deref(),
            end_str.as_deref(),
        ],
        |row| {
            let round_number: i64 = row.get(0)?;
            let input_tokens: i64 = row.get(1)?;
            let output_tokens: i64 = row.get(2)?;
            let latency_ms: i64 = row.get(3)?;
            let cache_read: i64 = row.get(4)?;
            let tool_count: i64 = row.get(5)?;
            let stop_reason: String = row.get(6)?;
            let agent_id: String = row.get(7)?;
            let started_at_str: String = row.get(8)?;
            let session_id: Option<String> = row.get(9)?;
            let model: String = row.get(10)?;

            let timestamp = parse_rfc3339(&started_at_str);
            let channel = channel_from_session(&session_id);
            let cache_str = if cache_read > 0 {
                format!(", {} cached", cache_read)
            } else {
                String::new()
            };

            Ok(ActivityRecord {
                timestamp,
                agent_id,
                activity_type: ActivityType::LlmCall,
                channel,
                description: format!(
                    "Round {} on {}: {} in / {} out tokens, {}ms, {} tools, {}{}",
                    round_number,
                    model,
                    input_tokens,
                    output_tokens,
                    latency_ms,
                    tool_count,
                    stop_reason,
                    cache_str
                ),
                duration_ms: Some(latency_ms),
                success: Some(true),
                metadata: None,
            })
        },
    )?;

    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

// ─── Postgres backend ──────────────────────────────────────────────────
//
// Mirror of the SQLite `query_tasks` / `query_tools` / `query_rounds`
// helpers above against `tokio_postgres`. Used by
// `crate::telemetry_postgres::PostgresTelemetryStore::query_activities`.
// Native `TIMESTAMPTZ` columns mean we bind `DateTime<Utc>` directly
// instead of round-tripping through RFC3339 strings.

#[cfg(feature = "postgres")]
pub async fn query_activities_postgres(
    conn: &deadpool_postgres::Object,
    agent_id: Option<&str>,
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
    verbose: bool,
) -> Result<Vec<ActivityRecord>> {
    let mut activities = pg_query_tasks(conn, agent_id, start_time, end_time).await?;
    activities.extend(pg_query_tools(conn, agent_id, start_time, end_time).await?);
    if verbose {
        activities.extend(pg_query_rounds(conn, agent_id, start_time, end_time).await?);
    }
    activities.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    Ok(activities)
}

#[cfg(feature = "postgres")]
async fn pg_query_tasks(
    conn: &deadpool_postgres::Object,
    agent_id: Option<&str>,
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
) -> Result<Vec<ActivityRecord>> {
    let sql = "SELECT
        t.agent_id,
        t.started_at,
        t.completed_at,
        t.session_id,
        (t.input_tokens + t.output_tokens) AS total_tokens,
        t.cost_usd,
        t.rounds,
        t.success,
        t.error_type,
        t.model,
        t.duration_ms
    FROM task_telemetry t
    WHERE ($1::text IS NULL OR t.agent_id = $1)
        AND ($2::timestamptz IS NULL OR t.started_at >= $2)
        AND ($3::timestamptz IS NULL OR t.started_at <= $3)
    ORDER BY t.started_at DESC";

    let rows = conn
        .query(sql, &[&agent_id, &start_time, &end_time])
        .await?;

    let mut out = Vec::with_capacity(rows.len() * 2);
    for row in rows {
        let agent_id: String = row.get(0);
        let started_at: DateTime<Utc> = row.get(1);
        let completed_at: Option<DateTime<Utc>> = row.get(2);
        let session_id: Option<String> = row.get(3);
        let total_tokens: i64 = row.get(4);
        let cost_usd: f64 = row.get(5);
        let rounds: i64 = row.get(6);
        let success: bool = row.get(7);
        let error_type: Option<String> = row.get(8);
        let model: String = row.get(9);
        let duration_ms: i64 = row.get(10);

        let channel = channel_from_session(&session_id);

        out.push(ActivityRecord {
            timestamp: started_at,
            agent_id: agent_id.clone(),
            activity_type: ActivityType::SessionStarted,
            channel: channel.clone(),
            description: format!("Task started on {} ({} rounds)", model, rounds),
            duration_ms: None,
            success: None,
            metadata: None,
        });

        if let Some(completed) = completed_at {
            let outcome = if success {
                "success".to_string()
            } else {
                error_type
                    .as_ref()
                    .map(|e| format!("failed: {e}"))
                    .unwrap_or_else(|| "failed".to_string())
            };

            let mut metadata = serde_json::Map::new();
            metadata.insert(
                "total_tokens".to_string(),
                serde_json::Value::Number(total_tokens.into()),
            );
            metadata.insert("cost_usd".to_string(), serde_json::Value::from(cost_usd));
            metadata.insert(
                "model".to_string(),
                serde_json::Value::String(model.clone()),
            );

            out.push(ActivityRecord {
                timestamp: completed,
                agent_id,
                activity_type: ActivityType::SessionCompleted,
                channel,
                description: format!(
                    "Task completed ({}) — {} tokens, ${:.4}",
                    outcome, total_tokens, cost_usd
                ),
                duration_ms: Some(duration_ms),
                success: Some(success),
                metadata: Some(serde_json::Value::Object(metadata)),
            });
        }
    }
    Ok(out)
}

#[cfg(feature = "postgres")]
async fn pg_query_tools(
    conn: &deadpool_postgres::Object,
    agent_id: Option<&str>,
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
) -> Result<Vec<ActivityRecord>> {
    let sql = "SELECT
        tool.tool_name,
        tool.success,
        tool.duration_ms,
        tool.round_number,
        task.agent_id,
        task.started_at,
        task.session_id
    FROM tool_telemetry tool
    JOIN task_telemetry task ON tool.task_telemetry_id = task.id
    WHERE ($1::text IS NULL OR task.agent_id = $1)
        AND ($2::timestamptz IS NULL OR task.started_at >= $2)
        AND ($3::timestamptz IS NULL OR task.started_at <= $3)
    ORDER BY task.started_at DESC, tool.round_number ASC";

    let rows = conn
        .query(sql, &[&agent_id, &start_time, &end_time])
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let tool_name: String = row.get(0);
        let success: bool = row.get(1);
        let duration_ms: i64 = row.get(2);
        let round_number: i64 = row.get(3);
        let agent_id: String = row.get(4);
        let started_at: DateTime<Utc> = row.get(5);
        let session_id: Option<String> = row.get(6);

        let channel = channel_from_session(&session_id);
        let status = if success { "" } else { " [FAILED]" };

        out.push(ActivityRecord {
            timestamp: started_at,
            agent_id,
            activity_type: ActivityType::ToolExecution,
            channel,
            description: format!(
                "Tool: {}{} ({}ms, round {})",
                tool_name, status, duration_ms, round_number
            ),
            duration_ms: Some(duration_ms),
            success: Some(success),
            metadata: None,
        });
    }
    Ok(out)
}

#[cfg(feature = "postgres")]
async fn pg_query_rounds(
    conn: &deadpool_postgres::Object,
    agent_id: Option<&str>,
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
) -> Result<Vec<ActivityRecord>> {
    let sql = "SELECT
        r.round_number,
        r.input_tokens,
        r.output_tokens,
        r.latency_ms,
        r.cache_read_tokens,
        r.tool_count,
        r.stop_reason,
        t.agent_id,
        t.started_at,
        t.session_id,
        t.model
    FROM round_telemetry r
    JOIN task_telemetry t ON r.task_telemetry_id = t.id
    WHERE ($1::text IS NULL OR t.agent_id = $1)
        AND ($2::timestamptz IS NULL OR t.started_at >= $2)
        AND ($3::timestamptz IS NULL OR t.started_at <= $3)
    ORDER BY t.started_at DESC, r.round_number ASC";

    let rows = conn
        .query(sql, &[&agent_id, &start_time, &end_time])
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let round_number: i64 = row.get(0);
        let input_tokens: i64 = row.get(1);
        let output_tokens: i64 = row.get(2);
        let latency_ms: i64 = row.get(3);
        let cache_read: i64 = row.get(4);
        let tool_count: i64 = row.get(5);
        let stop_reason: String = row.get(6);
        let agent_id: String = row.get(7);
        let started_at: DateTime<Utc> = row.get(8);
        let session_id: Option<String> = row.get(9);
        let model: String = row.get(10);

        let channel = channel_from_session(&session_id);
        let cache_str = if cache_read > 0 {
            format!(", {} cached", cache_read)
        } else {
            String::new()
        };

        out.push(ActivityRecord {
            timestamp: started_at,
            agent_id,
            activity_type: ActivityType::LlmCall,
            channel,
            description: format!(
                "Round {} on {}: {} in / {} out tokens, {}ms, {} tools, {}{}",
                round_number,
                model,
                input_tokens,
                output_tokens,
                latency_ms,
                tool_count,
                stop_reason,
                cache_str
            ),
            duration_ms: Some(latency_ms),
            success: Some(true),
            metadata: None,
        });
    }
    Ok(out)
}

/// Parse relative time strings like "1h", "30m", "7d".
pub fn parse_relative_time(input: &str) -> Result<DateTime<Utc>> {
    let input = input.trim();
    if input.is_empty() {
        return Err(eyre!("Empty time string"));
    }

    let (num_str, unit) = input.split_at(input.len() - 1);
    let num: i64 = num_str
        .parse()
        .map_err(|_| eyre!("Invalid number in time string: {}", input))?;

    let duration = match unit {
        "s" => Duration::seconds(num),
        "m" => Duration::minutes(num),
        "h" => Duration::hours(num),
        "d" => Duration::days(num),
        "w" => Duration::weeks(num),
        _ => return Err(eyre!("Invalid time unit '{}'. Use s/m/h/d/w", unit)),
    };

    Ok(Utc::now() - duration)
}

/// Parse absolute timestamps (RFC3339, or `YYYY-MM-DD[ HH:MM[:SS]]`).
pub fn parse_absolute_time(input: &str) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(input) {
        return Ok(dt.with_timezone(&Utc));
    }

    let formats = ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%d"];

    for format in &formats {
        if let Ok(naive) = NaiveDateTime::parse_from_str(input, format) {
            return Ok(DateTime::from_naive_utc_and_offset(naive, Utc));
        }
        if let Ok(naive_date) = chrono::NaiveDate::parse_from_str(input, format) {
            let naive = naive_date.and_hms_opt(0, 0, 0).unwrap();
            return Ok(DateTime::from_naive_utc_and_offset(naive, Utc));
        }
    }

    Err(eyre!("Could not parse timestamp: {}", input))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_relative_time() {
        let now = Utc::now();
        let one_hour_ago = parse_relative_time("1h").unwrap();
        let diff = now.signed_duration_since(one_hour_ago);
        assert!(diff.num_seconds() >= 3599 && diff.num_seconds() <= 3601);

        assert!(parse_relative_time("30m").is_ok());
        assert!(parse_relative_time("7d").is_ok());
        assert!(parse_relative_time("2w").is_ok());
        assert!(parse_relative_time("invalid").is_err());
    }

    #[test]
    fn test_parse_absolute_time() {
        assert!(parse_absolute_time("2024-01-15T10:30:00Z").is_ok());
        assert!(parse_absolute_time("2024-01-15 10:30:00").is_ok());
        assert!(parse_absolute_time("2024-01-15 10:30").is_ok());
        assert!(parse_absolute_time("2024-01-15").is_ok());
        assert!(parse_absolute_time("invalid").is_err());
    }
}
