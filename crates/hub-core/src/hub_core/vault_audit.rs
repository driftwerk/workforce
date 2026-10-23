//! Append-only JSONL audit log for vault writes.
//!
//! Records `set` and `delete` operations (reads are not logged — volume
//! would be enormous; per-entry `updated_at` covers the diagnostic case).
//! Each event is one JSON line: timestamp, actor tag, action, path,
//! success flag, and SHA-256-first-8 hex of the value plus its previous
//! value. Raw values are never written to disk.
//!
//! The log file lives next to the vault file (`<dir>/vault-audit.jsonl`)
//! and is opened with mode `0o600`. Concurrent writers are line-atomic
//! through the per-log mutex.
//!
//! **Rotation (F5, 2026-05-15).** When a write would push the live file
//! past [`DEFAULT_MAX_LOG_BYTES`], the current file is renamed to
//! `vault-audit.<unix-ts>.jsonl` and a fresh one is opened. The oldest
//! rotated copies are pruned so only [`DEFAULT_MAX_ROTATED_FILES`]
//! remain. This caps disk usage at roughly `(N+1) × max_bytes` and
//! prevents the disk-exhaustion vector flagged in audit finding F5.

use std::borrow::Cow;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Rotate when the live file would cross 50 MiB.
pub const DEFAULT_MAX_LOG_BYTES: u64 = 50 * 1024 * 1024;
/// Keep at most five rotated files alongside the live one.
pub const DEFAULT_MAX_ROTATED_FILES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    Set,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry<'a> {
    pub ts: DateTime<Utc>,
    pub actor: Cow<'a, str>,
    pub action: AuditAction,
    pub path: Cow<'a, str>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_sha256_first8: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_value_sha256_first8: Option<String>,
}

/// First 8 hex chars of `SHA-256(value)`. Lets the audit log answer
/// "same value as before?" without retaining the secret itself.
pub fn value_fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(&digest[..4])
}

/// Append-only JSONL audit log. One handle is shared across all vault
/// writes to a given file.
pub struct VaultAuditLog {
    path: PathBuf,
    file: Mutex<Option<File>>,
    max_bytes: u64,
    max_rotated: usize,
}

impl VaultAuditLog {
    /// Open (or prepare to open lazily on first write) an audit log at
    /// `path`. Uses the default rotation thresholds.
    pub fn open(path: PathBuf) -> Result<Self> {
        Self::open_with_rotation(path, DEFAULT_MAX_LOG_BYTES, DEFAULT_MAX_ROTATED_FILES)
    }

    /// Open with custom rotation thresholds. The parent directory is
    /// created if missing.
    pub fn open_with_rotation(path: PathBuf, max_bytes: u64, max_rotated: usize) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).wrap_err_with(|| {
                format!("Failed to create audit log directory {}", parent.display())
            })?;
        }
        Ok(Self {
            path,
            file: Mutex::new(None),
            max_bytes,
            max_rotated,
        })
    }

    /// Path to the on-disk log file (for diagnostics).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every entry in the log at `live_path` and its rotated copies, oldest
    /// first. Lines that don't parse (a write cut short) are skipped.
    pub fn read(live_path: &Path) -> Result<Vec<AuditEntry<'static>>> {
        let files: Vec<PathBuf> = rotated_logs(live_path)
            .into_iter()
            .chain(live_path.exists().then(|| live_path.to_path_buf()))
            .collect();
        let contents = files
            .iter()
            .map(|path| {
                std::fs::read_to_string(path)
                    .wrap_err_with(|| format!("read audit log {}", path.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut entries: Vec<AuditEntry<'static>> = contents
            .iter()
            .flat_map(|content| {
                content
                    .lines()
                    .filter_map(|line| serde_json::from_str(line).ok())
            })
            .collect();
        entries.sort_by_key(|entry| entry.ts);
        Ok(entries)
    }

    /// Append a single audit entry. Errors are logged via `tracing` and
    /// swallowed so audit failures never bubble up into vault write
    /// failures — losing an audit line is preferable to losing a secret
    /// rotation.
    pub fn record(&self, entry: AuditEntry<'_>) {
        if let Err(e) = self.try_record(&entry) {
            tracing::error!(
                error = %e,
                path = %entry.path,
                action = ?entry.action,
                "Failed to write vault audit entry"
            );
        }
    }

    fn try_record(&self, entry: &AuditEntry<'_>) -> Result<()> {
        let line = serde_json::to_string(entry).wrap_err("serialise audit entry")?;
        let pending_bytes = line.len() as u64 + 1; // + newline

        let mut guard = self.file.lock();

        let current_size = match guard.as_ref() {
            Some(f) => f.metadata().map(|m| m.len()).unwrap_or(0),
            None => std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0),
        };
        if current_size + pending_bytes > self.max_bytes {
            // Drop the live handle, rename the file out of the way, prune
            // oldest rotated copies, then fall through to the lazy-open
            // path which creates a fresh 0-byte file.
            guard.take();
            rotate_log(&self.path)?;
            prune_old_logs(&self.path, self.max_rotated);
        }

        if guard.is_none() {
            let mut opts = OpenOptions::new();
            opts.append(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let f = opts
                .open(&self.path)
                .wrap_err_with(|| format!("open audit log {}", self.path.display()))?;
            *guard = Some(f);
        }
        let file = guard
            .as_mut()
            .expect("audit log file just initialised above");
        writeln!(file, "{line}").wrap_err("write audit line")?;
        file.flush().wrap_err("flush audit log")?;
        Ok(())
    }
}

/// Rename `path` to `<stem>.<unix-nanos>.<ext>` (no-op if the file
/// doesn't exist). Nanosecond precision so back-to-back rotations under
/// abuse don't collide on a same-second timestamp and silently clobber
/// the earlier rotated file.
fn rotate_log(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("vault-audit");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("jsonl");
    let ts = Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or_else(|| Utc::now().timestamp_micros());
    let rotated = path.with_file_name(format!("{stem}.{ts}.{ext}"));
    std::fs::rename(path, &rotated)
        .wrap_err_with(|| format!("rotate audit log to {}", rotated.display()))?;
    Ok(())
}

/// Delete the oldest rotated logs so at most `max_keep` remain. Best
/// effort — errors are logged via tracing and otherwise ignored (losing
/// an old log line is not worth failing a vault write).
fn prune_old_logs(live_path: &Path, max_keep: usize) {
    let rotated = rotated_logs(live_path);
    let excess = rotated.len().saturating_sub(max_keep);
    rotated.into_iter().take(excess).for_each(|path| {
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!(path = %path.display(), error = %e, "Failed to prune rotated vault audit log");
        }
    });
}

/// Rotated copies (`<stem>.<timestamp>.<ext>`) of the log at `live_path`,
/// oldest first.
fn rotated_logs(live_path: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(stem)) = (
        live_path.parent(),
        live_path.file_stem().and_then(|s| s.to_str()),
    ) else {
        return Vec::new();
    };
    let ext = live_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("jsonl");
    let (prefix, suffix) = (format!("{stem}."), format!(".{ext}"));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rotated: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            let is_rotated =
                path != live_path && name.starts_with(&prefix) && name.ends_with(&suffix);
            let modified = entry.metadata().ok()?.modified().ok()?;
            is_rotated.then_some((modified, path))
        })
        .collect();
    rotated.sort_by_key(|(modified, _)| *modified);
    rotated.into_iter().map(|(_, path)| path).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|s| s.to_string())
            .collect()
    }

    fn entry<'a>(path: &'a str, value: &'a str) -> AuditEntry<'a> {
        AuditEntry {
            ts: Utc::now(),
            actor: "cli".into(),
            action: AuditAction::Set,
            path: path.into(),
            ok: true,
            value_sha256_first8: Some(value_fingerprint(value)),
            prev_value_sha256_first8: None,
        }
    }

    #[test]
    fn records_set_and_delete_with_actor_and_action() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("vault-audit.jsonl");
        let log = VaultAuditLog::open(log_path.clone()).unwrap();

        log.record(AuditEntry {
            ts: Utc::now(),
            actor: "user:kings".into(),
            action: AuditAction::Set,
            path: "skills/jira/api_token".into(),
            ok: true,
            value_sha256_first8: Some(value_fingerprint("secret-token")),
            prev_value_sha256_first8: None,
        });
        log.record(AuditEntry {
            ts: Utc::now(),
            actor: "cli".into(),
            action: AuditAction::Delete,
            path: "skills/jira/api_token".into(),
            ok: true,
            value_sha256_first8: None,
            prev_value_sha256_first8: Some(value_fingerprint("secret-token")),
        });

        let lines = read_lines(&log_path);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"action\":\"set\""));
        assert!(lines[0].contains("user:kings"));
        assert!(lines[1].contains("\"action\":\"delete\""));
    }

    #[test]
    fn records_failures_with_ok_false() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("vault-audit.jsonl");
        let log = VaultAuditLog::open(log_path.clone()).unwrap();

        log.record(AuditEntry {
            ts: Utc::now(),
            actor: "agent:agent-x".into(),
            action: AuditAction::Set,
            path: "engagement/overrides".into(),
            ok: false,
            value_sha256_first8: None,
            prev_value_sha256_first8: None,
        });

        let lines = read_lines(&log_path);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"ok\":false"));
    }

    #[test]
    fn never_records_raw_value() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("vault-audit.jsonl");
        let log = VaultAuditLog::open(log_path.clone()).unwrap();

        log.record(AuditEntry {
            ts: Utc::now(),
            actor: "cli".into(),
            action: AuditAction::Set,
            path: "secret/path".into(),
            ok: true,
            value_sha256_first8: Some(value_fingerprint("ultra-secret-value-XYZ")),
            prev_value_sha256_first8: None,
        });

        let raw = std::fs::read_to_string(&log_path).unwrap();
        assert!(!raw.contains("ultra-secret-value-XYZ"));
        assert!(raw.contains(&value_fingerprint("ultra-secret-value-XYZ")));
    }

    #[test]
    fn concurrent_writes_produce_intact_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("vault-audit.jsonl");
        let log = std::sync::Arc::new(VaultAuditLog::open(log_path.clone()).unwrap());

        let handles: Vec<_> = (0..16)
            .map(|i| {
                let log = std::sync::Arc::clone(&log);
                std::thread::spawn(move || {
                    let path = format!("k{i}");
                    log.record(AuditEntry {
                        ts: Utc::now(),
                        actor: "cli".into(),
                        action: AuditAction::Set,
                        path: path.as_str().into(),
                        ok: true,
                        value_sha256_first8: Some(value_fingerprint(&path)),
                        prev_value_sha256_first8: None,
                    });
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let lines = read_lines(&log_path);
        assert_eq!(lines.len(), 16);
        for line in lines {
            let _: serde_json::Value =
                serde_json::from_str(&line).expect("each line must be valid JSON");
        }
    }

    // ── F5: rotation ───────────────────────────────────────────────────

    /// Reads every `vault-audit.*.jsonl` in `dir` (live + rotated)
    /// concatenated, returning the union of path values logged. Used
    /// to assert rotation preserves content rather than truncating it.
    fn all_logged_paths(dir: &Path) -> Vec<String> {
        let mut paths: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                let name = p.file_name()?.to_str()?;
                if !name.starts_with("vault-audit.") || !name.ends_with(".jsonl") {
                    return None;
                }
                let content = std::fs::read_to_string(&p).ok()?;
                Some(
                    content
                        .lines()
                        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                        .filter_map(|v| v.get("path")?.as_str().map(str::to_owned))
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn rotates_when_threshold_crossed() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("vault-audit.jsonl");
        // 200 bytes ~= 1-2 entries; the third pushes us over.
        let log = VaultAuditLog::open_with_rotation(log_path.clone(), 200, 5).unwrap();

        let expected: Vec<String> = (0..5).map(|i| format!("path/{i}")).collect();
        for p in &expected {
            log.record(entry(p, "x"));
        }

        assert!(log_path.exists());

        // Rotated copies exist alongside the live file.
        let rotated_count = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with("vault-audit.")
                    && name.ends_with(".jsonl")
                    && name != "vault-audit.jsonl"
            })
            .count();
        assert!(rotated_count > 0, "expected at least one rotated file");

        // And no entry was lost across the rotation boundary.
        let mut expected_sorted = expected;
        expected_sorted.sort();
        assert_eq!(all_logged_paths(tmp.path()), expected_sorted);
    }

    #[test]
    fn read_returns_every_rotated_entry_oldest_first_and_skips_torn_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("vault-audit.jsonl");
        let log = VaultAuditLog::open_with_rotation(log_path.clone(), 200, 5).unwrap();
        let expected: Vec<String> = (0..5).map(|i| format!("path/{i}")).collect();
        for p in &expected {
            log.record(entry(p, "x"));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let mut live = OpenOptions::new().append(true).open(&log_path).unwrap();
        writeln!(live, "{{\"ts\":\"cut short").unwrap();

        let read: Vec<String> = VaultAuditLog::read(&log_path)
            .unwrap()
            .into_iter()
            .map(|e| e.path.into_owned())
            .collect();
        assert_eq!(read, expected);
    }

    #[test]
    fn reading_a_missing_log_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(VaultAuditLog::read(&tmp.path().join("vault-audit.jsonl"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn prunes_old_rotated_files() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("vault-audit.jsonl");
        // Force rotation every entry; keep only 2 rotated copies.
        let log = VaultAuditLog::open_with_rotation(log_path.clone(), 1, 2).unwrap();

        // Each entry crosses the 1-byte threshold immediately, producing
        // many rotated files in succession.
        for i in 0..6 {
            log.record(entry(&format!("path/{i}"), "x"));
            // Spread timestamps slightly so the sort order is stable.
            std::thread::sleep(std::time::Duration::from_millis(15));
        }

        let rotated_count = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with("vault-audit.")
                    && name.ends_with(".jsonl")
                    && name != "vault-audit.jsonl"
            })
            .count();
        // At most max_rotated rotated files survive — plus the live one.
        assert!(
            rotated_count <= 2,
            "expected ≤2 rotated files after pruning, got {rotated_count}"
        );
    }
}
